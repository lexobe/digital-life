use axum::{Json, Router, routing::post};
use serde_json::{Value, json};

use survive_server::{App, api, config::Config};

async fn listener(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    (format!("http://{address}"), task)
}

#[tokio::test]
async fn organism_switches_models_within_environment_and_pays_each_quote() {
    const LOW: &str = "gpt-6-luna";
    const HIGH: &str = "gpt-6.2-astra";
    const OUTSIDE: &str = "a-model-from-another-environment";
    let upstream = Router::new().route("/v1/responses", post(|Json(input): Json<Value>| async move {
        let model = input["model"].as_str().unwrap();
        let choices = match model {
            LOW => vec![HIGH],
            HIGH => vec![LOW, OUTSIDE],
            _ => panic!("unexpected upstream model"),
        };
        let output: Vec<_> = choices.iter().enumerate().map(|(i, choice)| json!({
            "type":"function_call", "call_id":format!("call-{model}-{i}"), "name":"choose_model",
            "arguments":json!({"model":choice}).to_string()
        })).collect();
        Json(json!({"object":"response","id":format!("resp-{model}"),"model":model,
                    "status":"completed","output":output,"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10}}))
    }));
    let (provider, upstream_task) = listener(upstream).await;
    let dir = tempfile::tempdir().unwrap();
    let owner = std::env::var("PATH").unwrap();
    let config = serde_json::from_value(json!({
        "data_dir":dir.path(),"listen":"127.0.0.1:0","env_listen":"127.0.0.1:0","test_mode":true,
        "creators":[{"id":"creator","token_env":"PATH"}],
        "gateway":{"prices":{(LOW):{"input":1,"output":5},(HIGH):{"input":80,"output":400},(OUTSIDE):{"input":1,"output":1}},
            "providers":[{"name":"fixture","base_url":format!("{provider}/v1"),"models":[LOW,HIGH,OUTSIDE]}]}
    })).unwrap();
    let app = App::new(config).unwrap();
    let (api, owner_task) = listener(api::owner_router(app.clone())).await;
    let (guest, guest_task) = listener(api::environment_router(app.clone())).await;
    let client = reqwest::Client::new();
    let environments = format!("{api}/api/v2/environments");
    let create = |ids: Value| {
        client
            .post(&environments)
            .bearer_auth(&owner)
            .json(
                &json!({"description":"Choose the model for your next turn.","allowed_models":ids}),
            )
            .send()
    };
    assert_eq!(create(json!(["unconfigured"])).await.unwrap().status(), 400);
    let env: Value = create(json!([LOW, HIGH]))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(env["environment"]["allowed_models"], json!([LOW, HIGH]));
    let members = format!(
        "{environments}/{}/organisms",
        env["environment"]["id"].as_str().unwrap()
    );
    let defaults = json!({"initial_energy":5_000_000,"lifetime_seconds":600});
    let birth = |payload: Value| {
        client
            .post(&members)
            .bearer_auth(&owner)
            .json(&payload)
            .send()
    };
    for rejected in [
        json!({"count":1,"defaults":{"initial_energy":10,"lifetime_seconds":10,"default_model":OUTSIDE},"overrides":[{"default_model":LOW}]}),
        json!({"count":2,"defaults":defaults,"overrides":[{},{"allowed_models":[LOW,OUTSIDE]}]}),
        json!({"count":2,"defaults":defaults,"overrides":[{},{"default_model":OUTSIDE}]}),
    ] {
        assert_eq!(birth(rejected).await.unwrap().status(), 400);
    }
    assert!(app.0.store.profiles(None).unwrap().is_empty());
    let born: Value =
        birth(json!({"count":2,"defaults":defaults,"overrides":[{},{"allowed_models":[HIGH]}]}))
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
    assert_eq!(born["organisms"][0]["default_model"], LOW);
    assert_eq!(born["organisms"][1]["default_model"], HIGH);
    let id = born["organisms"][0]["id"].as_str().unwrap();
    let key = app.0.store.agent_credential(id).unwrap();
    let restricted = app
        .0
        .store
        .agent_credential(born["organisms"][1]["id"].as_str().unwrap())
        .unwrap();
    for (token, expected) in [(&key, vec![LOW, HIGH]), (&restricted, vec![HIGH])] {
        for (path, field) in [
            ("/env/v2/models", "models"),
            ("/env/v2/self", "models"),
            ("/v1/models", "data"),
        ] {
            let body: Value = client
                .get(format!("{guest}{path}"))
                .bearer_auth(token)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            let ids: Vec<_> = body[field]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, expected);
        }
    }
    for (token, denied) in [(&key, OUTSIDE), (&restricted, LOW)] {
        for (path, body) in [
            ("/v1/responses", json!({"model":denied,"input":"hi"})),
            (
                "/v1/chat/completions",
                json!({"model":denied,"messages":[]}),
            ),
        ] {
            assert_eq!(
                client
                    .post(format!("{guest}{path}"))
                    .bearer_auth(token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                403
            );
        }
    }
    assert!(
        app.0
            .store
            .reserve_priced(
                id,
                "denied",
                "fixture",
                1,
                OUTSIDE,
                survive_core::EnergyPrice {
                    input: 1,
                    output: 1
                }
            )
            .is_err()
    );
    assert!(app.0.store.reservations(id).unwrap().is_empty());
    let work = dir.path().join("agents").join(id).join("work");
    let mut seed = survive_agent::Config::new(&guest, &key, work.clone());
    seed.max_output_tokens = 64;
    let mut agent = survive_agent::Agent::open(seed).unwrap();
    for (turn, next) in [(1, HIGH), (2, LOW)] {
        assert_eq!(
            agent.step().await.unwrap(),
            survive_agent::StepOutcome::Advanced { turn }
        );
        assert_eq!(
            std::fs::read_to_string(work.join("model.txt")).unwrap(),
            next
        );
    }
    let fees = app.0.store.reservations(id).unwrap();
    assert_eq!(fees.len(), 2);
    for (model, charge) in [(LOW, 7 + 3 * 5), (HIGH, 7 * 80 + 3 * 400)] {
        let entry = fees
            .iter()
            .find(|r| r.model.as_deref() == Some(model))
            .unwrap();
        let usage = entry.usage.as_ref().unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (7, 3));
        assert_eq!(entry.energy_charged, Some(charge));
    }
    assert_eq!(app.0.store.get_agent(id).unwrap().used, 1782);
    for task in [owner_task, guest_task, upstream_task] {
        task.abort();
    }
}

#[tokio::test]
async fn environment_birth_prices_report_snapshot_and_idempotent_grants() {
    let upstream=Router::new().route("/v1/chat/completions",post(|Json(input):Json<Value>|async move{
        assert_eq!(input["model"],"upstream-pro");
        Json(json!({"id":"chat-priced","object":"chat.completion","model":"upstream-pro","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150}}))
    }));
    let (provider, up_task) = listener(upstream).await;
    let dir = tempfile::tempdir().unwrap();
    let owner = std::env::var("PATH").unwrap();
    let config:Config=serde_json::from_value(json!({"data_dir":dir.path(),"listen":"127.0.0.1:0","env_listen":"127.0.0.1:0","test_mode":true,
        "creators":[{"id":"creator","token_env":"PATH"}],"gateway":{"prices":{"upstream-mini":{"input":1,"output":1},"upstream-pro":{"input":2,"output":3}},"providers":[{"name":"fixture","base_url":format!("{provider}/v1"),"models":["upstream-mini","upstream-pro"]}]}})).unwrap();
    let app = App::new(config).unwrap();
    let (api, owner_task) = listener(api::owner_router(app.clone())).await;
    let (guest, guest_task) = listener(api::environment_router(app.clone())).await;
    let client = reqwest::Client::new();
    let send = |path: String, token: &str, data: Value| {
        client.post(path).bearer_auth(token).json(&data).send()
    };
    let create = send(
        format!("{api}/api/v2/environments"),
        &owner,
        json!({"description":"Explore"}),
    )
    .await
    .unwrap();
    assert_eq!(create.status(), 201);
    let data: Value = create.json().await.unwrap();
    let eid = data["environment"]["id"].as_str().unwrap();
    let url = format!("{api}/api/v2/environments/{eid}/organisms");
    let result=send(url,&owner,json!({"count":2,"defaults":{"initial_energy":100000,"lifetime_seconds":180,"dna":"original"},"overrides":[{},{"allowed_models":["upstream-mini"],"network":{"peers":false}}]})).await.unwrap();
    assert_eq!(result.status(), 201);
    let born: Value = result.json().await.unwrap();
    assert_eq!(born["organisms"][0]["default_model"], "upstream-mini");
    let id = born["organisms"][0]["id"].as_str().unwrap();
    let second = born["organisms"][1]["id"].as_str().unwrap();
    let key = app.0.store.agent_credential(id).unwrap();
    let key2 = app.0.store.agent_credential(second).unwrap();
    let body = json!({"model":"upstream-pro","messages":[{"role":"user","content":"work"}],"max_tokens":64});
    assert_eq!(
        send(format!("{guest}/v1/chat/completions"), &key, body.clone())
            .await
            .unwrap()
            .status(),
        200
    );
    assert!(
        !send(format!("{guest}/v1/chat/completions"), &key2, body)
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(app.0.store.get_agent(id).unwrap().used, 350);
    let self_info: Value = client
        .get(format!("{guest}/env/v2/self"))
        .bearer_auth(&key2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(self_info["environment"]["description"], "Explore");
    assert_eq!(self_info["models"].as_array().unwrap().len(), 1);
    let workspace = dir.path().join("agents").join(id).join("work");
    assert_eq!(
        std::fs::read_to_string(workspace.join("dna.md")).unwrap(),
        "original"
    );
    let file = workspace.join("proof.bin");
    std::fs::write(&file, [0, 1, 2, 255]).unwrap();
    let payload = json!({"body":"artifact","files":["proof.bin"],"idempotency_key":"report-one"});
    let submit = send(format!("{guest}/env/v2/reports"), &key, payload.clone())
        .await
        .unwrap();
    assert_eq!(submit.status(), 200);
    let report: Value = submit.json().await.unwrap();
    let rid = report["report"]["id"].as_str().unwrap();
    assert_eq!(report["report"]["snapshots"][0]["data"], "AAEC/w==");
    std::fs::remove_file(file).unwrap();
    let repeated: Value = send(format!("{guest}/env/v2/reports"), &key, payload)
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(repeated, report);
    let url = format!("{api}/api/v2/organisms/{id}/grants");
    let grant =
        json!({"idempotency_key":"first","energy":19,"lifetime_seconds":30,"report_id":rid});
    for _ in 0..2 {
        let value: Value = send(url.clone(), &owner, grant.clone())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(value["life"]["remaining_lifetime_ms"], 210000);
        assert_eq!(value["life"]["energy_balance"], 99669);
    }
    assert_eq!(send(url.clone(), &key, grant).await.unwrap().status(), 403);
    let foreign = app
        .0
        .store
        .create_environment(
            "second-owner",
            serde_json::from_value(json!({"description":"private"})).unwrap(),
        )
        .unwrap();
    let foreign = app
        .0
        .store
        .birth_organisms(
            "second-owner",
            &foreign.id,
            serde_json::from_value(
                json!({"count":1,"defaults":{"initial_energy":10,"lifetime_seconds":5}}),
            )
            .unwrap(),
        )
        .unwrap();
    let url = format!("{api}/api/v2/organisms/{}/grants", foreign[0].id);
    assert!(
        !send(
            url,
            &owner,
            json!({"energy":1,"idempotency_key":"forbidden"})
        )
        .await
        .unwrap()
        .status()
        .is_success()
    );
    assert_eq!(
        client
            .get(format!("{api}/api/v2/organisms/{id}/reports/{rid}"))
            .bearer_auth(&owner)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        send(format!("{api}/api/v1/experiments"), &owner, json!({}))
            .await
            .unwrap()
            .status(),
        405
    );
    let overview = client
        .get(format!("{api}/api/v2/overview"))
        .bearer_auth(&owner)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!overview.contains(&foreign[0].id));
    assert!(!overview.contains("token_hash"));
    for task in [owner_task, guest_task, up_task] {
        task.abort();
    }
}
