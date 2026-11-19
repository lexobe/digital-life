//! Deterministic HTTP-body tests that also run when loopback sockets are denied.
use super::*;
use survive_core::{ExperimentInput, Reservation, ReservationStatus};

fn fixture() -> (Gateway, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let input: ExperimentInput = serde_json::from_value(json!({"name":"test","goal":"test","initial_prompt":"test","count":1,"initial_tokens":100_000,"reward_budget":0,"rules":[]})).unwrap();
    let (_, agents) = store.create_experiment("owner", input).unwrap();
    let config = GatewayConfig {
        providers: vec![ProviderConfig {
            name: "test".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key_env: None,
            models: std::collections::BTreeMap::from([("default".into(), "upstream-model".into())]),
            protocols: vec![Protocol::Chat, Protocol::Responses],
        }],
        ..GatewayConfig::default()
    };
    (
        Gateway::new(config, store).unwrap(),
        agents[0].0.id.clone(),
        dir,
    )
}

fn upstream(text: String) -> reqwest::Response {
    let body = reqwest::Body::wrap_stream(futures_util::stream::iter(
        text.into_bytes()
            .chunks(7)
            .map(|bytes| Ok::<_, Infallible>(Bytes::copy_from_slice(bytes)))
            .collect::<Vec<_>>(),
    ));
    axum::http::Response::builder()
        .header("content-type", "text/event-stream")
        .body(body)
        .unwrap()
        .into()
}
fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn completed(id: &str, usage: Value) -> Value {
    json!({"id":id,"object":"response","status":"completed","model":"upstream-model","output":[],"usage":usage})
}

async fn run_stream(
    g: &Gateway,
    agent: &str,
    protocol: Protocol,
    text: String,
    disconnected: bool,
) -> (Result<(), GatewayError>, String, Reservation) {
    let mut attempt = g
        .inner
        .reserve(
            agent,
            "test",
            100,
            "default",
            EnergyPrice {
                input: 1,
                output: 1,
            },
        )
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::channel::<Result<Bytes, Infallible>>(2);
    let consumer = tokio::spawn(async move {
        let mut text = String::new();
        if disconnected {
            return text;
        }
        while let Some(Ok(bytes)) = rx.recv().await {
            text.push_str(std::str::from_utf8(&bytes).unwrap());
        }
        text
    });
    let mut sender = Some(tx);
    let result = g
        .inner
        .stream(
            agent,
            protocol,
            "test",
            "default",
            false,
            upstream(text),
            &mut attempt,
            &mut sender,
        )
        .await;
    if result.is_err() && attempt.active {
        g.inner
            .finish(&mut attempt, None, None, false)
            .await
            .unwrap();
    }
    drop(sender);
    let text = consumer.await.unwrap();
    let reservation = g
        .inner
        .store
        .reservations(agent)
        .unwrap()
        .into_iter()
        .find(|r| r.id == attempt.id)
        .unwrap();
    (result, text, reservation)
}

#[tokio::test]
async fn stream_http_body_settles_actual_usage_even_above_reservation() {
    let (g, agent, _dir) = fixture();
    let text = format!(
        "{}{}data: [DONE]\n\n",
        event(json!({"id":"chat-1","choices":[{"delta":{"content":"hi"}}]})),
        event(
            json!({"id":"chat-1","usage":{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050}})
        )
    );
    let (result, wire, reservation) = run_stream(&g, &agent, Protocol::Chat, text, false).await;
    result.unwrap();
    assert!(wire.contains("[DONE]"));
    assert_eq!(reservation.status, ReservationStatus::Settled);
    assert_eq!(g.inner.store.get_agent(&agent).unwrap().used, 1050);
    assert!(
        g.inner
            .store
            .events(&agent, None, 100)
            .unwrap()
            .iter()
            .any(|e| e.kind == "tokens_settled" && e.data["over_reservation"] == 950)
    );
}

#[tokio::test]
async fn truncated_stream_retains_identified_unknown_attempt_for_recovery() {
    let (g, agent, _dir) = fixture();
    let text = event(
        json!({"type":"response.created","response":{"id":"resp-1","object":"response","model":"upstream-model","status":"in_progress","output":[]}}),
    );
    let (result, _, reservation) = run_stream(&g, &agent, Protocol::Responses, text, false).await;
    assert!(result.is_err());
    assert_eq!(reservation.status, ReservationStatus::Unknown);
    assert_eq!(reservation.upstream_id.as_deref(), Some("resp-1"));
    assert_eq!(g.inner.store.get_agent(&agent).unwrap().reserved, 100);
    assert_eq!(
        g.inner
            .store
            .get_response(&agent, "resp-1")
            .unwrap()
            .provider,
        "test"
    );
}

#[tokio::test]
async fn disconnected_client_does_not_prevent_response_terminal_settlement() {
    let (g, agent, _dir) = fixture();
    let text = event(
        json!({"type":"response.completed","response":completed("resp-1",json!({"input_tokens":5,"output_tokens":3}))}),
    );
    let (result, wire, reservation) = run_stream(&g, &agent, Protocol::Responses, text, true).await;
    result.unwrap();
    assert!(wire.is_empty());
    assert_eq!(reservation.status, ReservationStatus::Settled);
    let body = g.inner.store.get_response(&agent, "resp-1").unwrap().body;
    assert_eq!(body["status"], "completed");
    assert_eq!(body["model"], "default");
}

#[tokio::test]
async fn missing_stream_usage_keeps_reservation_and_successful_terminal_event() {
    let (g, agent, _dir) = fixture();
    let text =
        event(json!({"type":"response.completed","response":completed("resp-1",Value::Null)}));
    let (result, wire, reservation) =
        run_stream(&g, &agent, Protocol::Responses, text, false).await;
    result.unwrap();
    assert!(wire.contains("response.completed"));
    assert_eq!(reservation.status, ReservationStatus::Unknown);
}

#[tokio::test]
async fn item_and_call_ids_bind_before_their_event_is_forwarded() {
    let (g, agent, _dir) = fixture();
    let text = format!(
        "{}{}{}",
        event(
            json!({"type":"response.created","response":{"id":"resp-1","object":"response","status":"in_progress","output":[]}})
        ),
        event(
            json!({"type":"response.output_item.added","item":{"id":"item-1","call_id":"call-1","type":"function_call","name":"test","arguments":""}})
        ),
        event(
            json!({"type":"response.completed","response":completed("resp-1",json!({"input_tokens":5,"output_tokens":3}))})
        )
    );
    let (result, wire, _) = run_stream(&g, &agent, Protocol::Responses, text, false).await;
    result.unwrap();
    assert!(wire.contains("call-1"));
    for id in ["item-1", "call-1"] {
        assert_eq!(
            g.inner.store.get_response(&agent, id).unwrap().provider,
            "test"
        );
    }
    let prepared=g.inner.prepare(&agent,Protocol::Responses,json!({"model":"default","input":[{"type":"function_call_output","call_id":"call-1","output":"ok"}]})).await.unwrap();
    assert_eq!(prepared.sticky.as_deref(), Some("test"));
    assert!(!prepared.retryable);
}

#[tokio::test]
async fn late_usage_reconciliation_validates_id_owner_and_settles_exactly_once() {
    let (g, agent, _dir) = fixture();
    let text =
        event(json!({"type":"response.completed","response":completed("resp-1",Value::Null)}));
    let (_, _, reservation) = run_stream(&g, &agent, Protocol::Responses, text, false).await;
    let binding = g.inner.store.get_response(&agent, "resp-1").unwrap();
    let usage = json!({"input_tokens":400,"output_tokens":10,"total_tokens":410});
    assert!(
        !g.inner
            .recover(
                &reservation,
                &binding,
                Protocol::Responses,
                "default",
                completed("wrong-id", usage.clone())
            )
            .await
            .unwrap()
    );
    let mut wrong_owner = binding.clone();
    wrong_owner.agent_id = "another-agent".into();
    assert!(
        !g.inner
            .recover(
                &reservation,
                &wrong_owner,
                Protocol::Responses,
                "default",
                completed("resp-1", usage.clone())
            )
            .await
            .unwrap()
    );
    assert!(
        !g.inner
            .recover(
                &reservation,
                &binding,
                Protocol::Responses,
                "default",
                completed("resp-1", json!({"input_tokens":10}))
            )
            .await
            .unwrap()
    );
    let body = completed("resp-1", usage);
    assert!(
        g.inner
            .recover(
                &reservation,
                &binding,
                Protocol::Responses,
                "default",
                body.clone()
            )
            .await
            .unwrap()
    );
    // A concurrent stale snapshot may verify the same settlement again; the
    // atomic Store operation must emit and charge it exactly once.
    assert!(
        g.inner
            .recover(&reservation, &binding, Protocol::Responses, "default", body)
            .await
            .unwrap()
    );
    assert_eq!(
        g.inner
            .store
            .events(&agent, None, 100)
            .unwrap()
            .iter()
            .filter(|event| event.kind == "tokens_settled")
            .count(),
        1
    );
    assert!(
        g.inner
            .store
            .unknown_reservations(None, 16)
            .unwrap()
            .is_empty()
    );
    let agent = g.inner.store.get_agent(&agent).unwrap();
    assert_eq!((agent.used, agent.reserved), (410, 0));
}

#[tokio::test]
async fn response_get_is_scoped_without_any_upstream_network() {
    let (g, agent, _dir) = fixture();
    g.inner
        .bind(&agent, "test", &completed("resp-1", Value::Null), "default")
        .await
        .unwrap();
    assert!(g.response(&agent, "resp-1").await.is_ok());
    assert!(matches!(
        g.response("other-agent", "resp-1").await,
        Err(GatewayError::NotFound)
    ));
    assert!(
        g.response(&agent, "https://elsewhere.invalid/resp-1")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn request_limits_and_provider_credentials_fail_before_network() {
    let (mut g, agent, _dir) = fixture();
    Arc::get_mut(&mut g.inner).unwrap().config.max_request_bytes = 256;
    let request = json!({"model":"default","messages":[{"role":"user","content":"x".repeat(300)}]});
    assert!(matches!(
        g.completion(&agent, Protocol::Chat, request).await,
        Err(GatewayError::InvalidRequest(_))
    ));
    assert!(matches!(
        g.completion(
            &agent,
            Protocol::Chat,
            json!({"model":"forbidden","messages":[]})
        )
        .await,
        Err(GatewayError::ModelNotAllowed)
    ));
    let mut config = g.inner.config.clone();
    config.providers[0].api_key_env = Some(format!("SURVIVE_ABSENT_{}", new_id()));
    let unavailable = Gateway::new(config, g.inner.store.clone()).unwrap();
    assert!(matches!(
        unavailable
            .completion(
                &agent,
                Protocol::Chat,
                json!({"model":"default","messages":[]})
            )
            .await,
        Err(GatewayError::Unavailable)
    ));
    assert!(g.inner.store.reservations(&agent).unwrap().is_empty());
}

#[test]
fn usage_and_secret_redaction_are_strict() {
    assert!(
        parse_usage(
            &json!({"usage":{"input_tokens":u64::MAX,"output_tokens":1}}),
            Protocol::Responses
        )
        .is_none()
    );
    assert!(
        parse_usage(
            &json!({"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":1}}),
            Protocol::Responses
        )
        .is_none()
    );
    assert_eq!(GatewayConfig::default().global_concurrency, 16);
    let (mut g, _, _dir) = fixture();
    Arc::get_mut(&mut g.inner)
        .unwrap()
        .redactions
        .push("fake-secret".into());
    let mut body = json!({"message":"fake-secret at http://127.0.0.1:1/v1/responses","nested":[{"model":"upstream-model"}]});
    g.inner.sanitize(&mut body, Some("default"));
    assert!(!body.to_string().contains("fake-secret") && !body.to_string().contains("127.0.0.1"));
    assert_eq!(body["nested"][0]["model"], "default");
}

#[tokio::test]
async fn event_history_redacts_escaped_secrets_before_persistence() {
    let (mut g, agent, _dir) = fixture();
    Arc::get_mut(&mut g.inner)
        .unwrap()
        .redactions
        .push("fake-secret".into());
    g.inner
        .event(
            &agent,
            "gateway.upstream_error",
            json!({"body":r#"{"error":"f\u0061ke-secret at http://127.0.0.1:1/v1"}"#}),
        )
        .await
        .unwrap();
    let events = g.inner.store.events(&agent, None, 100).unwrap();
    let event = events
        .iter()
        .find(|e| e.kind == "gateway.upstream_error")
        .unwrap();
    let decoded: Value = serde_json::from_str(event.data["body"].as_str().unwrap()).unwrap();
    assert!(!decoded.to_string().contains("fake-secret"));
    assert!(!decoded.to_string().contains("127.0.0.1"));
}

#[test]
fn production_memory_and_reconciliation_defaults() {
    let config: GatewayConfig = serde_json::from_value(json!({})).unwrap();
    assert_eq!(config.global_concurrency, 16);
    assert_eq!(config.max_request_bytes, 262_144);
    assert_eq!(config.max_response_bytes, 4_194_304);
    assert_eq!(config.max_sse_event_bytes, 262_144);
    assert_eq!(config.stream_buffer_events, 8);
    assert_eq!(config.reconcile_max_attempts, 16);
    assert_eq!(config.reconcile_time_budget_ms, 20_000);
}

#[tokio::test]
async fn reconciliation_rotates_bounded_batches_past_unverifiable_entries() {
    let (mut g, agent, _dir) = fixture();
    Arc::get_mut(&mut g.inner)
        .unwrap()
        .config
        .reconcile_max_attempts = 2;
    for id in ["r1", "r2", "r3"] {
        g.inner.store.reserve(&agent, id, "test", 100).unwrap();
        g.inner
            .store
            .mark_unknown(id, Some(format!("response-{id}")))
            .unwrap();
    }
    // These unknowns have IDs but no binding, so no network calls are allowed.
    for expected in ["r2", "r1", "r3"] {
        assert_eq!(g.reconcile_unknown().await.unwrap(), 0);
        assert_eq!(
            g.inner.reconciliation.lock().await.as_deref(),
            Some(expected)
        );
    }
    let reservations = g.inner.store.reservations(&agent).unwrap();
    assert!(
        reservations
            .iter()
            .all(|r| r.status == ReservationStatus::Unknown)
    );
    assert_eq!(g.inner.store.get_agent(&agent).unwrap().reserved, 300);
}

#[tokio::test]
async fn concurrent_reconciliation_does_not_queue_another_pass() {
    let (g, _, _dir) = fixture();
    let _running = g.inner.reconciliation.lock().await;
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(100), g.reconcile_unknown())
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[test]
fn invalid_reconciliation_limits_are_rejected() {
    let (g, _, _dir) = fixture();
    for (attempts, budget) in [(0, 20_000), (1025, 20_000), (16, 0), (16, 60_001)] {
        let mut config = g.inner.config.clone();
        config.reconcile_max_attempts = attempts;
        config.reconcile_time_budget_ms = budget;
        assert!(Gateway::new(config, g.inner.store.clone()).is_err());
    }
}
