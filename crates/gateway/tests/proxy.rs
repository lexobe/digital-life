use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};
use survive_core::{ExperimentInput, ReservationStatus, Store};
use survive_gateway::{Gateway, GatewayConfig, GatewayError, Protocol, ProviderConfig};
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle};

#[derive(Clone, Debug)]
struct Captured {
    path: String,
    headers: HeaderMap,
    body: Value,
}

enum Plan {
    Json(StatusCode, Value),
    Raw(StatusCode, &'static str, Vec<u8>),
    Redirect(String),
    Sse(Vec<(Duration, Vec<u8>)>),
    Wait {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        body: Value,
    },
}

struct MockState {
    plans: Mutex<VecDeque<Plan>>,
    seen: Arc<Mutex<Vec<Captured>>>,
}
struct Mock {
    url: String,
    seen: Arc<Mutex<Vec<Captured>>>,
    task: JoinHandle<()>,
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mock {
    async fn start(plans: Vec<Plan>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(vec![]));
        let state = Arc::new(MockState {
            plans: Mutex::new(plans.into()),
            seen: seen.clone(),
        });
        let app = Router::new().fallback(handle).with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, seen, task }
    }
    fn provider(&self, name: &str) -> ProviderConfig {
        ProviderConfig {
            name: name.into(),
            base_url: self.url.clone(),
            api_key_env: None,
            models: BTreeMap::from([("default".into(), format!("deployment-{name}"))]),
            protocols: vec![Protocol::Chat, Protocol::Responses],
        }
    }
    fn requests(&self) -> Vec<Captured> {
        self.seen.lock().unwrap().clone()
    }
}

async fn handle(State(state): State<Arc<MockState>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, 2 * 1024 * 1024).await.unwrap();
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    state.seen.lock().unwrap().push(Captured {
        path: parts.uri.path().into(),
        headers: parts.headers,
        body,
    });
    let plan = state.plans.lock().unwrap().pop_front();
    match plan {
        Some(Plan::Json(status, body)) => (status, Json(body)).into_response(),
        Some(Plan::Raw(status, content_type, bytes)) => Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(Body::from(bytes))
            .unwrap(),
        Some(Plan::Redirect(location)) => Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header("location", location)
            .body(Body::empty())
            .unwrap(),
        Some(Plan::Sse(chunks)) => {
            let chunks: VecDeque<_> = chunks.into();
            let stream = stream::unfold(chunks, |mut chunks| async move {
                let (delay, bytes) = chunks.pop_front()?;
                tokio::time::sleep(delay).await;
                Some((Ok::<_, Infallible>(Bytes::from(bytes)), chunks))
            });
            Response::builder()
                .header("content-type", "text/event-stream; charset=utf-8")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        Some(Plan::Wait {
            entered,
            release,
            body,
        }) => {
            entered.notify_one();
            release.notified().await;
            Json(body).into_response()
        }
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"unexpected mock request"})),
        )
            .into_response(),
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    agents: Vec<String>,
}
impl Fixture {
    fn new(tokens: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("ledger.redb")).unwrap();
        let input: ExperimentInput = serde_json::from_value(json!({"name":"gateway test","goal":"test proxy","initial_prompt":"hello","count":2,"initial_tokens":tokens,"reward_budget":0,"model":"default","rules":[]})).unwrap();
        let (_, agents) = store.create_experiment("owner", input).unwrap();
        Self {
            _dir: dir,
            store,
            agents: agents.into_iter().map(|(agent, _)| agent.id).collect(),
        }
    }
    fn gateway(&self, providers: Vec<ProviderConfig>) -> Gateway {
        Gateway::new(config(providers), self.store.clone()).unwrap()
    }
    fn statuses(&self) -> Vec<ReservationStatus> {
        self.store
            .reservations(&self.agents[0])
            .unwrap()
            .into_iter()
            .map(|r| r.status)
            .collect()
    }
}

fn config(providers: Vec<ProviderConfig>) -> GatewayConfig {
    GatewayConfig {
        providers,
        default_output_tokens: 16,
        max_output_tokens: 128,
        minimum_input_reservation: 8,
        request_timeout_ms: 3000,
        connect_timeout_ms: 250,
        ..GatewayConfig::default()
    }
}
fn chat_request() -> Value {
    json!({"model":"default","messages":[{"role":"user","content":"hello"}]})
}
fn chat_body() -> Value {
    json!({"id":"chatcmpl-one","object":"chat.completion","model":"deployment-primary","choices":[{"index":0,"message":{"role":"assistant","content":"hello","tool_calls":[{"id":"call-1","type":"function","function":{"name":"inspect","arguments":"{\"path\":\"/work\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10,"prompt_tokens_details":{"cached_tokens":2}},"extra":{"preserved":true}})
}
fn responses_body(id: &str) -> Value {
    json!({"id":id,"object":"response","model":"deployment-fallback","status":"completed","output":[{"id":format!("item-{id}"),"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":11,"output_tokens":4,"total_tokens":15}})
}
fn ok(body: Value) -> Plan {
    Plan::Json(StatusCode::OK, body)
}
fn sse(event: Option<&str>, body: &Value) -> String {
    format!(
        "{}data: {body}\n\n",
        event.map(|e| format!("event: {e}\n")).unwrap_or_default()
    )
}
fn instant_sse(text: String) -> Plan {
    Plan::Sse(vec![(Duration::ZERO, text.into_bytes())])
}
async fn json_body(response: Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap()
}
async fn text_body(response: Response) -> String {
    String::from_utf8(
        to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap()
}
async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("condition did not become true");
}

#[tokio::test]
async fn chat_proxies_tools_routes_models_and_settles_actual_usage() {
    let upstream = Mock::start(vec![ok(chat_body())]).await;
    let f = Fixture::new(10_000);
    let gateway = f.gateway(vec![upstream.provider("primary")]);
    let mut request = chat_request();
    request["tools"] =
        json!([{"type":"function","function":{"name":"inspect","parameters":{"type":"object"}}}]);
    request["temperature"] = json!(0.25);
    let response = json_body(
        gateway
            .completion(&f.agents[0], Protocol::Chat, request)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(response["model"], "default");
    assert_eq!(
        response["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
        "{\"path\":\"/work\"}"
    );
    assert_eq!(response["extra"]["preserved"], true);
    let captured = upstream.requests();
    assert_eq!(captured[0].path, "/v1/chat/completions");
    assert_eq!(captured[0].body["model"], "deployment-primary");
    assert_eq!(captured[0].body["max_completion_tokens"], 16);
    assert_eq!(captured[0].body["temperature"], 0.25);
    assert!(captured[0].headers.get("authorization").is_none());
    let agent = f.store.get_agent(&f.agents[0]).unwrap();
    assert_eq!((agent.used, agent.reserved), (10, 0));
    assert_eq!(f.statuses(), vec![ReservationStatus::Settled]);
    assert!(
        f.store
            .events(&f.agents[0], None, 1000)
            .unwrap()
            .iter()
            .any(|e| e.kind == "gateway.upstream_body"
                && e.data["body"]
                    .as_str()
                    .unwrap()
                    .contains("deployment-primary"))
    );
    assert_eq!(gateway.models()["data"][0]["id"], "default");
}

#[tokio::test]
async fn stateless_failover_keeps_ambiguous_attempt_reserved() {
    let first = Mock::start(vec![Plan::Json(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error":"ambiguous"}),
    )])
    .await;
    let second = Mock::start(vec![ok(chat_body())]).await;
    let f = Fixture::new(10_000);
    let g = f.gateway(vec![first.provider("primary"), second.provider("fallback")]);
    g.completion(&f.agents[0], Protocol::Chat, chat_request())
        .await
        .unwrap();
    let reservations = f.store.reservations(&f.agents[0]).unwrap();
    assert_eq!(reservations.len(), 2);
    let unknown = reservations
        .iter()
        .find(|r| r.status == ReservationStatus::Unknown)
        .unwrap();
    assert_eq!(unknown.provider, "primary");
    assert_eq!(
        f.store.get_agent(&f.agents[0]).unwrap().reserved,
        unknown.amount
    );
    assert_eq!(f.store.get_agent(&f.agents[0]).unwrap().used, 10);
}

#[tokio::test]
async fn explicit_rejection_releases_and_responses_reference_stays_with_fallback() {
    let first = Mock::start(vec![Plan::Json(
        StatusCode::TOO_MANY_REQUESTS,
        json!({"error":"rate limited"}),
    )])
    .await;
    let second = Mock::start(vec![
        ok(responses_body("resp-one")),
        ok(responses_body("resp-two")),
        Plan::Json(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error":"unavailable"}),
        ),
    ])
    .await;
    let f = Fixture::new(10_000);
    let g = f.gateway(vec![first.provider("primary"), second.provider("fallback")]);
    g.completion(
        &f.agents[0],
        Protocol::Responses,
        json!({"model":"default","input":"hello"}),
    )
    .await
    .unwrap();
    assert!(f.statuses().contains(&ReservationStatus::Released));
    let body = json_body(g.response(&f.agents[0], "resp-one").await.unwrap()).await;
    assert_eq!(body["model"], "default");
    assert!(matches!(
        g.response(&f.agents[1], "resp-one").await,
        Err(GatewayError::NotFound)
    ));
    assert!(matches!(
        g.response(&f.agents[0], "item-resp-one").await,
        Err(GatewayError::NotFound)
    ));
    let request = json!({"model":"default","input":"next","previous_response_id":"resp-one"});
    assert!(matches!(
        g.completion(&f.agents[1], Protocol::Responses, request.clone())
            .await,
        Err(GatewayError::NotFound)
    ));
    g.completion(&f.agents[0], Protocol::Responses, request.clone())
        .await
        .unwrap();
    assert!(
        g.completion(&f.agents[0], Protocol::Responses, request)
            .await
            .is_err()
    );
    assert_eq!(first.requests().len(), 1);
    assert_eq!(second.requests().len(), 3);
    assert_eq!(
        second.requests()[1].body["previous_response_id"],
        "resp-one"
    );
    assert_eq!(second.requests()[0].path, "/v1/responses");
}

#[tokio::test]
async fn unknown_item_references_and_cross_provider_references_are_rejected() {
    let upstream = Mock::start(vec![
        ok(responses_body("resp-one")),
        ok(responses_body("resp-two")),
    ])
    .await;
    let f = Fixture::new(10_000);
    let g = f.gateway(vec![upstream.provider("primary")]);
    g.completion(
        &f.agents[0],
        Protocol::Responses,
        json!({"model":"default","input":"hi"}),
    )
    .await
    .unwrap();
    let input = json!({"model":"default","input":[{"type":"item_reference","id":"item-resp-one"}]});
    g.completion(&f.agents[0], Protocol::Responses, input)
        .await
        .unwrap();
    let input = json!({"model":"default","input":[{"type":"item_reference","id":"unknown"}]});
    assert!(matches!(
        g.completion(&f.agents[0], Protocol::Responses, input).await,
        Err(GatewayError::NotFound)
    ));
    f.store
        .bind_response(survive_core::ResponseBinding {
            agent_id: f.agents[0].clone(),
            response_id: "other".into(),
            provider: "other-provider".into(),
            body: responses_body("other"),
            created_at: 0,
        })
        .unwrap();
    let input = json!({"model":"default","previous_response_id":"other","input":[{"type":"item_reference","id":"item-resp-one"}]});
    assert!(matches!(
        g.completion(&f.agents[0], Protocol::Responses, input).await,
        Err(GatewayError::InvalidRequest(_))
    ));
    assert_eq!(upstream.requests().len(), 2);
}

#[tokio::test]
async fn missing_partial_negative_overflowing_and_inconsistent_usage_are_unknown() {
    let usages = vec![
        Value::Null,
        json!({"prompt_tokens":7}),
        json!({"prompt_tokens":-1,"completion_tokens":3}),
        json!({"prompt_tokens":u64::MAX,"completion_tokens":1}),
        json!({"prompt_tokens":7,"completion_tokens":3,"total_tokens":2}),
    ];
    let plans = usages
        .iter()
        .map(|usage| {
            let mut body = chat_body();
            body["usage"] = usage.clone();
            ok(body)
        })
        .collect();
    let upstream = Mock::start(plans).await;
    let f = Fixture::new(100_000);
    let g = f.gateway(vec![upstream.provider("primary")]);
    for _ in usages {
        g.completion(&f.agents[0], Protocol::Chat, chat_request())
            .await
            .unwrap();
    }
    assert!(
        f.statuses()
            .iter()
            .all(|status| *status == ReservationStatus::Unknown)
    );
    assert_eq!(f.store.get_agent(&f.agents[0]).unwrap().used, 0);
    assert!(f.store.get_agent(&f.agents[0]).unwrap().reserved > 0);
}

#[tokio::test]
async fn chat_sse_preserves_tool_deltas_and_forces_usage_reporting() {
    let content = format!(
        ": keepalive\r\n\r\n{}{}data: [DONE]\n\n",
        sse(
            None,
            &json!({"id":"chat-one","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":\"雪\"}"}}]}}]})
        ),
        sse(
            None,
            &json!({"id":"chat-one","choices":[],"usage":{"prompt_tokens":9,"completion_tokens":5,"total_tokens":14}})
        )
    );
    let chunks = content
        .as_bytes()
        .chunks(3)
        .map(|bytes| (Duration::ZERO, bytes.to_vec()))
        .collect();
    let upstream = Mock::start(vec![Plan::Sse(chunks)]).await;
    let f = Fixture::new(10_000);
    let g = f.gateway(vec![upstream.provider("primary")]);
    let mut request = chat_request();
    request["stream"] = json!(true);
    request["stream_options"] = json!({"include_usage":false});
    let response = g
        .completion(&f.agents[0], Protocol::Chat, request)
        .await
        .unwrap();
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let content = text_body(response).await;
    assert!(
        content.contains("keepalive")
            && content.contains("雪")
            && content.ends_with("data: [DONE]\n\n")
    );
    assert_eq!(
        upstream.requests()[0].body["stream_options"]["include_usage"],
        true
    );
    assert_eq!(f.store.get_agent(&f.agents[0]).unwrap().used, 14);
    assert_eq!(f.statuses(), vec![ReservationStatus::Settled]);
}

#[tokio::test]
async fn response_stream_binds_before_exposing_id_and_persists_terminal_snapshot() {
    let created = json!({"type":"response.created","response":{"id":"resp-stream","object":"response","status":"in_progress","model":"deployment-primary","output":[]}});
    let done = json!({"type":"response.completed","response":responses_body("resp-stream")});
    let upstream = Mock::start(vec![Plan::Sse(vec![
        (
            Duration::ZERO,
            sse(Some("response.created"), &created).into_bytes(),
        ),
        (
            Duration::from_millis(50),
            sse(Some("response.completed"), &done).into_bytes(),
        ),
    ])])
    .await;
    let f = Fixture::new(10_000);
    let g = f.gateway(vec![upstream.provider("primary")]);
    let response = g
        .completion(
            &f.agents[0],
            Protocol::Responses,
            json!({"model":"default","input":"hi","stream":true}),
        )
        .await
        .unwrap();
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&first).contains("resp-stream"));
    assert_eq!(
        f.store
            .get_response(&f.agents[0], "resp-stream")
            .unwrap()
            .provider,
        "primary"
    );
    while let Some(bytes) = stream.next().await {
        bytes.unwrap();
    }
    assert_eq!(
        f.store
            .get_response(&f.agents[0], "resp-stream")
            .unwrap()
            .body["status"],
        "completed"
    );
    assert_eq!(f.store.get_agent(&f.agents[0]).unwrap().used, 15);
    assert!(
        f.store
            .events(&f.agents[0], None, 1000)
            .unwrap()
            .iter()
            .filter(|e| e.kind == "gateway.sse")
            .count()
            >= 2
    );
}

#[tokio::test]
async fn interrupted_sse_never_refunds_or_fails_over_after_events() {
    let stream = format!(
        "{}data: {{\"id\":",
        sse(
            None,
            &json!({"id":"chat-one","choices":[{"delta":{"content":"partial"}}]})
        )
    );
    let first = Mock::start(vec![instant_sse(stream)]).await;
    let second = Mock::start(vec![ok(chat_body())]).await;
    let f = Fixture::new(10_000);
    let g = f.gateway(vec![first.provider("primary"), second.provider("fallback")]);
    let mut request = chat_request();
    request["stream"] = json!(true);
    let body = text_body(
        g.completion(&f.agents[0], Protocol::Chat, request)
            .await
            .unwrap(),
    )
    .await;
    assert!(body.contains("partial") && body.contains("event: error"));
    assert!(!body.contains("[DONE]"));
    assert_eq!(f.statuses(), vec![ReservationStatus::Unknown]);
    assert!(second.requests().is_empty());
}

#[tokio::test]
async fn client_disconnect_still_drains_and_settles_stream() {
    let first = sse(
        None,
        &json!({"id":"chat-one","choices":[{"delta":{"content":"hello"}}]}),
    );
    let last = format!(
        "{}data: [DONE]\n\n",
        sse(
            None,
            &json!({"id":"chat-one","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3}})
        )
    );
    let upstream = Mock::start(vec![Plan::Sse(vec![
        (Duration::ZERO, first.into_bytes()),
        (Duration::from_millis(80), last.into_bytes()),
    ])])
    .await;
    let f = Fixture::new(10_000);
    let g = f.gateway(vec![upstream.provider("primary")]);
    let mut request = chat_request();
    request["stream"] = json!(true);
    let response = g
        .completion(&f.agents[0], Protocol::Chat, request)
        .await
        .unwrap();
    drop(response);
    eventually(|| f.store.get_agent(&f.agents[0]).unwrap().used == 10).await;
    assert_eq!(f.statuses(), vec![ReservationStatus::Settled]);
}

#[tokio::test]
async fn cancellation_keeps_concurrency_and_accounting_until_attempt_finishes() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let upstream = Mock::start(vec![
        Plan::Wait {
            entered: entered.clone(),
            release: release.clone(),
            body: chat_body(),
        },
        ok(chat_body()),
    ])
    .await;
    let f = Fixture::new(10_000);
    let mut cfg = config(vec![upstream.provider("primary")]);
    cfg.per_agent_concurrency = 1;
    let g = Gateway::new(cfg, f.store.clone()).unwrap();
    let worker = g.clone();
    let id = f.agents[0].clone();
    let caller =
        tokio::spawn(async move { worker.completion(&id, Protocol::Chat, chat_request()).await });
    entered.notified().await;
    caller.abort();
    assert!(matches!(
        g.completion(&f.agents[0], Protocol::Chat, chat_request())
            .await,
        Err(GatewayError::Busy)
    ));
    release.notify_one();
    eventually(|| f.store.get_agent(&f.agents[0]).unwrap().used == 10).await;
    eventually(|| f.statuses() == vec![ReservationStatus::Settled]).await;
    tokio::task::yield_now().await;
    g.completion(&f.agents[0], Protocol::Chat, chat_request())
        .await
        .unwrap();
    assert_eq!(f.store.get_agent(&f.agents[0]).unwrap().used, 20);
}

#[tokio::test]
async fn global_concurrency_bounds_requests_from_distinct_agents() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let upstream = Mock::start(vec![Plan::Wait {
        entered: entered.clone(),
        release: release.clone(),
        body: chat_body(),
    }])
    .await;
    let f = Fixture::new(10_000);
    let mut cfg = config(vec![upstream.provider("primary")]);
    cfg.global_concurrency = 1;
    let g = Gateway::new(cfg, f.store.clone()).unwrap();
    let worker = g.clone();
    let id = f.agents[0].clone();
    let caller =
        tokio::spawn(async move { worker.completion(&id, Protocol::Chat, chat_request()).await });
    entered.notified().await;
    assert!(matches!(
        g.completion(&f.agents[1], Protocol::Chat, chat_request())
            .await,
        Err(GatewayError::Busy)
    ));
    release.notify_one();
    caller.await.unwrap().unwrap();
    assert!(f.store.reservations(&f.agents[1]).unwrap().is_empty());
}

#[tokio::test]
async fn request_model_output_and_balance_limits_precede_network() {
    let upstream = Mock::start(vec![]).await;
    let f = Fixture::new(1);
    let g = f.gateway(vec![upstream.provider("primary")]);
    let mut request = chat_request();
    request["model"] = json!("forbidden");
    assert!(matches!(
        g.completion(&f.agents[0], Protocol::Chat, request).await,
        Err(GatewayError::ModelNotAllowed)
    ));
    for output in [json!(0), json!(-1), json!(129), json!(1.5), json!("10")] {
        let mut request = chat_request();
        request["max_tokens"] = output;
        assert!(matches!(
            g.completion(&f.agents[0], Protocol::Chat, request).await,
            Err(GatewayError::InvalidRequest(_))
        ));
    }
    let mut request = chat_request();
    request["n"] = json!(u64::MAX);
    assert!(matches!(
        g.completion(&f.agents[0], Protocol::Chat, request).await,
        Err(GatewayError::InvalidRequest(_))
    ));
    assert!(matches!(
        g.completion(&f.agents[0], Protocol::Chat, chat_request())
            .await,
        Err(GatewayError::InsufficientTokens)
    ));
    assert!(upstream.requests().is_empty());
    assert!(f.statuses().is_empty());
}

#[tokio::test]
async fn configured_missing_credentials_do_not_prevent_startup_or_hit_upstream() {
    let upstream = Mock::start(vec![]).await;
    let f = Fixture::new(10_000);
    let mut provider = upstream.provider("missing");
    provider.api_key_env = Some(format!("SURVIVE_TEST_ABSENT_{}", survive_core::new_id()));
    let g = f.gateway(vec![provider]);
    let error = g
        .completion(&f.agents[0], Protocol::Chat, chat_request())
        .await
        .unwrap_err();
    assert_eq!(error.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json_body(error.into_response()).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("credentials")
    );
    assert!(!body.to_string().contains(&upstream.url));
    assert_eq!(g.models()["data"], json!([]));
    assert!(upstream.requests().is_empty());
    assert!(f.statuses().is_empty());
    Gateway::new(GatewayConfig::default(), f.store.clone()).unwrap();
}

#[tokio::test]
async fn body_and_event_limits_retain_ambiguous_usage() {
    let upstream = Mock::start(vec![
        Plan::Raw(StatusCode::OK, "application/json", vec![b'x'; 2048]),
        instant_sse(format!("data: {}\n\n", "x".repeat(512))),
    ])
    .await;
    let f = Fixture::new(10_000);
    let mut cfg = config(vec![upstream.provider("primary")]);
    cfg.max_response_bytes = 1024;
    cfg.max_sse_event_bytes = 128;
    let g = Gateway::new(cfg, f.store.clone()).unwrap();
    assert!(
        g.completion(&f.agents[0], Protocol::Chat, chat_request())
            .await
            .is_err()
    );
    let mut request = chat_request();
    request["stream"] = json!(true);
    assert!(
        text_body(
            g.completion(&f.agents[0], Protocol::Chat, request)
                .await
                .unwrap()
        )
        .await
        .contains("event: error")
    );
    assert_eq!(f.statuses().len(), 2);
    assert!(
        f.statuses()
            .iter()
            .all(|s| *s == ReservationStatus::Unknown)
    );
}

#[tokio::test]
async fn connection_failure_is_released_and_redirects_are_never_followed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let redirect_target = Mock::start(vec![ok(chat_body())]).await;
    let upstream = Mock::start(vec![
        ok(chat_body()),
        Plan::Redirect(format!("{}/chat/completions", redirect_target.url)),
    ])
    .await;
    let f = Fixture::new(10_000);
    let mut dead = upstream.provider("dead");
    dead.base_url = format!("http://{addr}/v1");
    let g = f.gateway(vec![dead, upstream.provider("primary")]);
    g.completion(&f.agents[0], Protocol::Chat, chat_request())
        .await
        .unwrap();
    assert!(f.statuses().contains(&ReservationStatus::Released));
    let g = f.gateway(vec![upstream.provider("primary")]);
    assert!(
        g.completion(&f.agents[0], Protocol::Chat, chat_request())
            .await
            .is_err()
    );
    assert_eq!(upstream.requests().len(), 2);
    assert!(redirect_target.requests().is_empty());
}

#[tokio::test]
async fn full_body_read_timeout_is_unknown_without_refund() {
    let upstream = Mock::start(vec![Plan::Sse(vec![(
        Duration::from_millis(300),
        b"data: [DONE]\n\n".to_vec(),
    )])])
    .await;
    let f = Fixture::new(10_000);
    let mut cfg = config(vec![upstream.provider("primary")]);
    cfg.request_timeout_ms = 60;
    let g = Gateway::new(cfg, f.store.clone()).unwrap();
    let mut request = chat_request();
    request["stream"] = json!(true);
    let body = text_body(
        g.completion(&f.agents[0], Protocol::Chat, request)
            .await
            .unwrap(),
    )
    .await;
    assert!(body.contains("event: error"));
    assert_eq!(f.statuses(), vec![ReservationStatus::Unknown]);
}

#[test]
fn example_embedded_configuration_deserializes_and_invalid_limits_fail() {
    #[derive(serde::Deserialize)]
    struct Server {
        gateway: GatewayConfig,
    }
    let server: Server = toml::from_str(include_str!("../example.toml")).unwrap();
    assert_eq!(server.gateway.providers.len(), 2);
    assert_eq!(server.gateway.max_request_bytes, 262_144);
    assert_eq!(server.gateway.max_response_bytes, 4_194_304);
    assert_eq!(server.gateway.max_sse_event_bytes, 262_144);
    assert_eq!(server.gateway.stream_buffer_events, 8);
    assert_eq!(server.gateway.global_concurrency, 16);
    assert_eq!(
        server.gateway.providers[0].api_key_env.as_deref(),
        Some("SURVIVE_PRIMARY_API_KEY")
    );
    let f = Fixture::new(100);
    let cfg = GatewayConfig {
        global_concurrency: 0,
        ..GatewayConfig::default()
    };
    assert!(Gateway::new(cfg, f.store).is_err());
}

#[tokio::test]
async fn late_responses_usage_is_retrieved_from_owner_route_and_settled_once() {
    let mut initial = responses_body("resp-late");
    initial["usage"] = Value::Null;
    let upstream = Mock::start(vec![
        ok(initial),
        ok(responses_body("wrong-id")),
        ok(responses_body("resp-late")),
    ])
    .await;
    let f = Fixture::new(10_000);
    let g = f.gateway(vec![upstream.provider("primary")]);
    g.completion(
        &f.agents[0],
        Protocol::Responses,
        json!({"model":"default","input":"hello"}),
    )
    .await
    .unwrap();
    assert_eq!(f.statuses(), vec![ReservationStatus::Unknown]);
    assert!(matches!(
        g.response(&f.agents[1], "resp-late").await,
        Err(GatewayError::NotFound)
    ));
    assert_eq!(upstream.requests().len(), 1);
    assert_eq!(g.reconcile_unknown().await.unwrap(), 0);
    assert_eq!(f.statuses(), vec![ReservationStatus::Unknown]);
    assert_eq!(g.reconcile_unknown().await.unwrap(), 1);
    assert_eq!(g.reconcile_unknown().await.unwrap(), 0);
    assert_eq!(upstream.requests().len(), 3);
    assert_eq!(upstream.requests()[1].path, "/v1/responses/resp-late");
    assert_eq!(f.store.get_agent(&f.agents[0]).unwrap().used, 15);
    assert_eq!(f.store.get_agent(&f.agents[0]).unwrap().reserved, 0);
    assert_eq!(f.statuses(), vec![ReservationStatus::Settled]);
}

#[tokio::test]
async fn only_explicitly_stored_chat_is_eligible_for_late_usage_retrieval() {
    let mut initial = chat_body();
    initial["usage"] = Value::Null;
    let upstream = Mock::start(vec![ok(initial.clone()), ok(initial), ok(chat_body())]).await;
    let f = Fixture::new(10_000);
    let g = f.gateway(vec![upstream.provider("primary")]);
    g.completion(&f.agents[0], Protocol::Chat, chat_request())
        .await
        .unwrap();
    assert_eq!(g.reconcile_unknown().await.unwrap(), 0);
    let mut request = chat_request();
    request["store"] = json!(true);
    g.completion(&f.agents[1], Protocol::Chat, request)
        .await
        .unwrap();
    assert_eq!(g.reconcile_unknown().await.unwrap(), 1);
    assert_eq!(
        upstream.requests()[2].path,
        "/v1/chat/completions/chatcmpl-one"
    );
    assert_eq!(f.statuses(), vec![ReservationStatus::Unknown]);
    assert_eq!(f.store.get_agent(&f.agents[1]).unwrap().used, 10);
}

#[tokio::test]
async fn reconciliation_bounds_http_attempts_and_resumes_at_next_reservation() {
    let upstream = Mock::start(vec![
        ok(responses_body("resp-1")),
        ok(responses_body("resp-2")),
        ok(responses_body("resp-3")),
    ])
    .await;
    let f = Fixture::new(10_000);
    let mut cfg = config(vec![upstream.provider("primary")]);
    cfg.reconcile_max_attempts = 2;
    let g = Gateway::new(cfg, f.store.clone()).unwrap();
    for number in 1..=3 {
        let id = format!("reservation-{number}");
        let response_id = format!("resp-{number}");
        f.store.reserve(&f.agents[0], &id, "primary", 100).unwrap();
        f.store
            .mark_unknown(&id, Some(response_id.clone()))
            .unwrap();
        let mut body = responses_body(&response_id);
        body["usage"] = Value::Null;
        body["model"] = json!("default");
        f.store
            .bind_response(survive_core::ResponseBinding {
                agent_id: f.agents[0].clone(),
                response_id,
                provider: "primary".into(),
                body,
                created_at: 0,
            })
            .unwrap();
    }
    assert_eq!(g.reconcile_unknown().await.unwrap(), 2);
    assert_eq!(upstream.requests().len(), 2);
    assert_eq!(g.reconcile_unknown().await.unwrap(), 1);
    assert_eq!(upstream.requests().len(), 3);
    assert_eq!(g.reconcile_unknown().await.unwrap(), 0);
    assert_eq!(f.store.get_agent(&f.agents[0]).unwrap().used, 45);
}

#[tokio::test]
async fn reconciliation_shared_deadline_never_refunds_or_fetches_next_id() {
    let upstream = Mock::start(vec![Plan::Wait {
        entered: Arc::new(Notify::new()),
        release: Arc::new(Notify::new()),
        body: responses_body("resp-1"),
    }])
    .await;
    let f = Fixture::new(10_000);
    let mut cfg = config(vec![upstream.provider("primary")]);
    cfg.reconcile_time_budget_ms = 200;
    let g = Gateway::new(cfg, f.store.clone()).unwrap();
    for number in 1..=2 {
        let id = format!("reservation-{number}");
        let response_id = format!("resp-{number}");
        f.store.reserve(&f.agents[0], &id, "primary", 100).unwrap();
        f.store
            .mark_unknown(&id, Some(response_id.clone()))
            .unwrap();
        let mut body = responses_body(&response_id);
        body["usage"] = Value::Null;
        body["model"] = json!("default");
        f.store
            .bind_response(survive_core::ResponseBinding {
                agent_id: f.agents[0].clone(),
                response_id,
                provider: "primary".into(),
                body,
                created_at: 0,
            })
            .unwrap();
    }
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), g.reconcile_unknown())
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(upstream.requests().len(), 1);
    assert!(
        f.statuses()
            .iter()
            .all(|status| *status == ReservationStatus::Unknown)
    );
    assert_eq!(f.store.get_agent(&f.agents[0]).unwrap().reserved, 200);
}

#[tokio::test]
async fn model_choice_prices_each_failover_attempt_and_enforces_individual_permissions() {
    use survive_core::{EnergyPrice, runtime_clock};

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("ledger")).unwrap();
    let env = store
        .create_environment(
            "owner",
            serde_json::from_value(json!({
                "description":"Report your progress to the creator.",
                "allowed_models":["default","big"]
            }))
            .unwrap(),
        )
        .unwrap();
    let organisms = store
        .birth_organisms(
            "owner",
            &env.id,
            serde_json::from_value(json!({
                "count":2, "defaults":{"initial_energy":10000,"lifetime_seconds":10},
                "overrides":[{},{"allowed_models":["default"]}]
            }))
            .unwrap(),
        )
        .unwrap();
    let primary = Mock::start(vec![Plan::Json(
        StatusCode::TOO_MANY_REQUESTS,
        json!({"error":"busy"}),
    )])
    .await;
    let fallback = Mock::start(vec![ok(chat_body())]).await;
    let mut providers = vec![primary.provider("primary"), fallback.provider("fallback")];
    for p in &mut providers {
        p.models.insert("big".into(), format!("large-{}", p.name));
    }
    let price = EnergyPrice {
        input: 2,
        output: 5,
    };
    let mut cfg = config(providers);
    cfg.prices = BTreeMap::from([
        (
            "default".into(),
            EnergyPrice {
                input: 1,
                output: 1,
            },
        ),
        ("big".into(), price),
    ]);
    let gw = Gateway::new(cfg, store.clone()).unwrap();
    let mut request = chat_request();
    request["model"] = json!("big");
    let restricted = store.organism_profile(&organisms[1].id).unwrap();
    let catalog = gw.organism_catalog(&env, &restricted.birth);
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog[0]["id"], "default");
    assert!(matches!(
        gw.completion(&organisms[1].id, Protocol::Chat, request.clone())
            .await,
        Err(GatewayError::ModelNotAllowed)
    ));
    assert!(primary.requests().is_empty());
    let body = json_body(
        gw.completion(&organisms[0].id, Protocol::Chat, request)
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(body["model"], "big");
    assert_eq!(primary.requests()[0].body["model"], "large-primary");
    assert_eq!(fallback.requests()[0].body["model"], "large-fallback");
    let context = survive_core::environment_context_message(&env.description);
    for route in [&primary, &fallback] {
        assert_eq!(route.requests()[0].body["messages"][0], context);
    }
    let rs = store.reservations(&organisms[0].id).unwrap();
    assert_eq!(rs.len(), 2);
    assert!(
        rs.iter()
            .all(|r| r.model.as_deref() == Some("big") && r.price == Some(price))
    );
    assert_eq!(
        rs.iter()
            .filter(|r| r.status == ReservationStatus::Released)
            .count(),
        1
    );
    let settled = rs
        .iter()
        .find(|r| r.status == ReservationStatus::Settled)
        .unwrap();
    assert_eq!(settled.usage.as_ref().unwrap().total(), Some(10));
    assert_eq!(settled.energy_charged, Some(7 * 2 + 3 * 5));
    assert_eq!(store.get_agent(&organisms[0].id).unwrap().used, 29);

    // Expire lifetime in the clock while the persisted Agent state is still queued.
    let (boot, mono) = runtime_clock().unwrap();
    store
        .observe_runtime(&organisms[0].id, true, &boot, mono, 100)
        .unwrap();
    store
        .observe_runtime(&organisms[0].id, true, &boot, mono + 10000, 10100)
        .unwrap();
    assert!(matches!(
        gw.completion(
            &organisms[0].id,
            Protocol::Responses,
            json!({"model":"default","input":"hello"})
        )
        .await,
        Err(GatewayError::Inactive)
    ));
    assert_eq!(primary.requests().len(), 1);
}

#[tokio::test]
async fn reconciliation_after_restart_uses_the_requested_model_price_at_reservation() {
    use survive_core::EnergyPrice;

    let mut initial = responses_body("resp-held");
    initial["usage"] = Value::Null;
    let upstream = Mock::start(vec![ok(initial), ok(responses_body("resp-held"))]).await;
    let f = Fixture::new(10000);
    let mut cfg = config(vec![upstream.provider("primary")]);
    let old_price = EnergyPrice {
        input: 2,
        output: 7,
    };
    cfg.prices.insert("default".into(), old_price);
    let gw = Gateway::new(cfg.clone(), f.store.clone()).unwrap();
    gw.completion(
        &f.agents[0],
        Protocol::Responses,
        json!({"model":"default","input":"hello"}),
    )
    .await
    .unwrap();
    assert_eq!(f.statuses(), vec![ReservationStatus::Unknown]);
    drop(gw);
    cfg.prices.insert(
        "default".into(),
        EnergyPrice {
            input: 80,
            output: 90,
        },
    );
    let replacement = Gateway::new(cfg, f.store.clone()).unwrap();
    assert_eq!(replacement.reconcile_unknown().await.unwrap(), 1);
    assert_eq!(replacement.reconcile_unknown().await.unwrap(), 0);
    let rs = f.store.reservations(&f.agents[0]).unwrap();
    assert_eq!(rs[0].price, Some(old_price));
    assert_eq!(rs[0].energy_charged, Some(11 * 2 + 4 * 7));
    assert_eq!(f.store.get_agent(&f.agents[0]).unwrap().used, 50);
}

#[tokio::test]
async fn custom_responses_core_receives_its_environment_on_every_call() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("state")).unwrap();
    let env = store
        .create_environment(
            "owner",
            serde_json::from_value(json!({
                "description":"固定环境：observe the blue square.","allowed_models":["default"]
            }))
            .unwrap(),
        )
        .unwrap();
    let agents = store
        .birth_organisms(
            "owner",
            &env.id,
            serde_json::from_value(json!({
                "count":1,"defaults":{"initial_energy":60000,"lifetime_seconds":120}
            }))
            .unwrap(),
        )
        .unwrap();
    let upstream = Mock::start(vec![
        ok(responses_body("resp-first")),
        ok(responses_body("resp-next")),
    ])
    .await;
    let gateway = Gateway::new(config(vec![upstream.provider("primary")]), store.clone()).unwrap();
    let authoritative = survive_core::environment_context_message(&env.description);
    for request in [
        json!({"model":"default","input":"custom core input","truncation":"auto","store":true}),
        json!({"model":"default","input":[authoritative,{"role":"user","content":"continue"}],
               "previous_response_id":"resp-first","instructions":"answer my pending conversation"}),
    ] {
        json_body(
            gateway
                .completion(&agents[0].id, Protocol::Responses, request)
                .await
                .unwrap(),
        )
        .await;
    }
    let captured = upstream.requests();
    assert_eq!(
        captured[0].body["input"][1],
        json!({"role":"user","content":"custom core input"})
    );
    assert_eq!(captured[0].body["truncation"], "disabled");
    assert_eq!(captured[1].body["previous_response_id"], "resp-first");
    for call in captured {
        let items = call.body["input"].as_array().unwrap();
        assert_eq!(items.first(), Some(&authoritative));
        assert_eq!(items.iter().filter(|m| **m == authoritative).count(), 1);
    }
}
