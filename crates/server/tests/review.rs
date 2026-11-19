//! Review regressions: all HTTP tests exercise the router in-process, without
//! listeners, deployment, credentials, or upstream/model calls.
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode},
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Barrier},
    time::Duration,
};
use survive_core::{
    AgentState, ExperimentInput, Store, SubmissionInput, SubmissionStatus, Usage, token_hash,
};
use survive_gateway::Gateway;
use survive_server::{App, Inner, api, config::Config, lifecycle};
use tokio::sync::Mutex;
use tower::ServiceExt;

const OWNER: &str = "synthetic-review-owner-token-000001";

fn experiment(count: u32) -> ExperimentInput {
    serde_json::from_value(json!({
        "name":"review", "goal":"produce a valid artifact", "initial_prompt":"work autonomously",
        "count":count,"initial_tokens":100,"reward_budget":50,
        "rules":[{"id":"done","instructions":"is the artifact valid?","tokens":50}]
    }))
    .unwrap()
}

fn input(key: &str) -> SubmissionInput {
    SubmissionInput {
        rule_id: "done".into(),
        idempotency_key: key.into(),
        description: "artifact".into(),
        files: vec![],
        occurrence_id: None,
    }
}

fn app(data: &Path, source: Option<&Path>) -> App {
    let config: Config = serde_json::from_value(json!({
        "data_dir":data,"listen":"127.0.0.1:0","env_listen":"127.0.0.1:0","test_mode":true,
        "source_dir":source,"creators":[{"id":"owner","token_env":"REVIEW_UNUSED"}],
        "gateway":{"providers":[{"name":"unused","base_url":"http://127.0.0.1:9/v1","models":{"default":"unused"}}]},
        "judge":{"api_key_env":"REVIEW_UNUSED"}
    })).unwrap();
    let store = Store::open(data.join("review.redb")).unwrap();
    let gateway = Gateway::new(config.gateway.clone(), store.clone()).unwrap();
    // Construct with synthetic credentials so the tests never read actual keys
    // or mutate the process-global environment.
    App(Arc::new(Inner {
        desktop: Default::default(),
        config,
        store,
        gateway,
        sandbox: None,
        service_client: reqwest::Client::builder().no_proxy().build().unwrap(),
        creators: BTreeMap::from([("owner".into(), token_hash(OWNER))]),
        lifecycle_lock: Mutex::new(()),
        runtime: tokio::sync::RwLock::new(survive_server::console::RuntimeReadiness::initial(true)),
    }))
}

#[test]
fn a_stale_depletion_update_must_not_overwrite_a_late_reward() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).unwrap();
    let (_, agents) = store.create_experiment("owner", experiment(1)).unwrap();
    let id = &agents[0].0.id;
    let submission = store
        .accept_submission(id, input("one"), json!({"artifact":"frozen"}))
        .unwrap();
    store.reserve(id, "spend", "mock", 100).unwrap();
    store
        .settle(
            "spend",
            Usage {
                input_tokens: 100,
                output_tokens: 0,
            },
            None,
        )
        .unwrap();
    let stale_snapshot = store.get_agent(id).unwrap();
    assert_eq!(stale_snapshot.balance(), 0);

    // lifecycle::tick reads zero, awaits sandbox.status/stop, and meanwhile the
    // evaluator commits an award and transitions the agent back to Queued.
    store
        .finish_submission(&submission.id, true, json!({"approved":true}), false)
        .unwrap();
    assert_eq!(store.get_agent(id).unwrap().state, AgentState::Queued);
    store
        .set_agent_state(&stale_snapshot.id, AgentState::Depleted, None)
        .unwrap();
    let current = store.get_agent(id).unwrap();
    assert_eq!(current.balance(), 50);
    assert_ne!(
        current.state,
        AgentState::Depleted,
        "stale lifecycle write strands a funded agent; tick never starts positive-balance Depleted agents"
    );
}

#[test]
fn reward_deadline_must_be_checked_at_the_award_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).unwrap();
    let mut seed = experiment(1);
    seed.rules[0].expires_after_secs = 1;
    let (_, agents) = store.create_experiment("owner", seed).unwrap();
    let submission = store
        .accept_submission(
            &agents[0].0.id,
            input("slow-judge"),
            json!({"artifact":"frozen"}),
        )
        .unwrap();
    assert!(store.claim_submission(&submission.id).unwrap());
    // Represents a judge/evidence request begun before the deadline, returning
    // afterward. evaluate() passes expired=false on its success path.
    std::thread::sleep(Duration::from_millis(1100));
    let result = store
        .finish_submission(&submission.id, true, json!({"approved":true}), false)
        .unwrap();
    assert_eq!(
        result.status,
        SubmissionStatus::Expired,
        "slow evaluation awarded after the configured deadline"
    );
    assert_eq!(store.get_agent(&agents[0].0.id).unwrap().granted, 100);
}

#[test]
fn concurrent_awards_and_repeated_finish_preserve_the_shared_reward_budget() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).unwrap();
    let (experiment, agents) = store.create_experiment("owner", experiment(2)).unwrap();
    let submissions: Vec<_> = agents
        .iter()
        .map(|(a, _)| {
            store
                .accept_submission(&a.id, input("one"), json!({"artifact":"frozen"}))
                .unwrap()
        })
        .collect();
    let barrier = Arc::new(Barrier::new(3));
    let threads: Vec<_> = submissions
        .iter()
        .map(|submission| {
            let store = store.clone();
            let id = submission.id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .finish_submission(&id, true, json!({"approved":true}), false)
                    .unwrap()
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(
        results
            .iter()
            .filter(|s| s.status == SubmissionStatus::Awarded)
            .count(),
        1
    );
    for submission in submissions {
        store
            .finish_submission(&submission.id, true, json!({"duplicate":true}), false)
            .unwrap();
    }
    assert_eq!(
        store
            .get_experiment(&experiment.id)
            .unwrap()
            .reward_remaining,
        0
    );
    assert_eq!(
        store
            .list_agents(Some(&experiment.id))
            .unwrap()
            .iter()
            .map(|a| a.granted)
            .sum::<u64>(),
        250
    );
}

#[tokio::test]
async fn environment_binding_and_immutable_config_survive_spoofed_headers() {
    let temp = tempfile::tempdir().unwrap();
    let app = app(temp.path(), None);
    let (exp, agents) = app
        .0
        .store
        .create_experiment("owner", experiment(2))
        .unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", agents[0].1).parse().unwrap(),
    );
    headers.insert("x-forwarded-for", agents[0].0.source_ip.parse().unwrap());
    let other_peer = format!("{}:1234", agents[1].0.source_ip).parse().unwrap();
    assert_eq!(
        api::environment_agent(&app, &headers, other_peer)
            .await
            .unwrap_err()
            .0,
        StatusCode::FORBIDDEN
    );
    let correct_peer = format!("{}:1234", agents[0].0.source_ip).parse().unwrap();
    assert_eq!(
        api::environment_agent(&app, &headers, correct_peer)
            .await
            .unwrap()
            .id,
        agents[0].0.id
    );
    let before = serde_json::to_value(&exp.config).unwrap();
    for method in ["PUT", "PATCH", "POST"] {
        let request = Request::builder()
            .method(method)
            .uri(format!("/api/v1/experiments/{}", exp.id))
            .header("authorization", format!("Bearer {OWNER}"))
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"goal":"replace goal","initial_prompt":"replace prompt"}).to_string(),
            ))
            .unwrap();
        let response = api::owner_router(app.clone())
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
    assert_eq!(
        serde_json::to_value(app.0.store.get_experiment(&exp.id).unwrap().config).unwrap(),
        before
    );
}

async fn born(app: &App) -> (survive_core::Agent, String) {
    let store = &app.0.store;
    let env = store
        .create_environment(
            "owner",
            serde_json::from_value(json!({"description":"test"})).unwrap(),
        )
        .unwrap();
    let mut batch = store
        .birth_organisms(
            "owner",
            &env.id,
            serde_json::from_value(
                json!({"count":1,"defaults":{"initial_energy":100000,"lifetime_seconds":300}}),
            )
            .unwrap(),
        )
        .unwrap();
    let agent = batch.pop().unwrap();
    let profile = store.organism_profile(&agent.id).unwrap();
    lifecycle::seed_organism(app, &agent, &profile)
        .await
        .unwrap();
    let credential = store.agent_credential(&agent.id).unwrap();
    (agent, credential)
}

#[cfg(unix)]
#[tokio::test]
async fn non_regular_evidence_must_be_rejected_without_blocking_open() {
    let temp = tempfile::tempdir().unwrap();
    let app = app(temp.path(), None);
    let (individual, credential) = born(&app).await;
    let fifo = temp
        .path()
        .join("agents")
        .join(&individual.id)
        .join("work/evidence.fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let request = Request::builder()
        .method("POST")
        .uri("/env/v2/reports")
        .header("authorization", format!("Bearer {}", credential))
        .header("content-type", "application/json")
        .extension(ConnectInfo(
            "127.0.0.1:1234".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(Body::from(
            json!({"idempotency_key":"fifo","body":"fifo","files":["evidence.fifo"]}).to_string(),
        ))
        .unwrap();
    let router = api::environment_router(app);
    let mut request = tokio::spawn(async move { router.oneshot(request).await.unwrap() });
    let first = tokio::time::timeout(Duration::from_millis(500), &mut request).await;
    let blocked = first.is_err();
    let response = match first {
        Ok(response) => response.unwrap(),
        Err(_) => {
            // Release the blocked reader so a failing regression never hangs
            // Tokio shutdown or leaves a background worker behind.
            let _unblock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&fifo)
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), request)
                .await
                .unwrap()
                .unwrap()
        }
    };
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        !blocked,
        "snapshot opens a FIFO before checking regular-file metadata; an authenticated agent can exhaust blocking workers without upstream/model calls"
    );
}

#[tokio::test]
async fn seeded_agent_source_must_build_with_the_shipped_lockfile() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let app = app(temp.path(), Some(workspace));
    let (individual, _) = born(&app).await;
    let source = temp
        .path()
        .join("agents")
        .join(&individual.id)
        .join("work/source");
    for path in [
        "Cargo.toml",
        "Cargo.lock",
        "crates/agent/src/lib.rs",
        "crates/agent/src/main.rs",
        "crates/core/src/models.rs",
        "crates/core/src/store.rs",
    ] {
        assert!(source.join(path).exists(), "missing {path}");
    }
    let output = tokio::process::Command::new(env!("CARGO"))
        .arg("build")
        .arg("--locked")
        .arg("--offline")
        .args(["-p", "survive-agent", "--bin", "survive-agent"])
        .arg("--manifest-path")
        .arg(source.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(temp.path().join("build"))
        .output()
        .await
        .unwrap();
    if !output.status.success() {
        // Separately establish whether the package has complete sources. This
        // may rewrite only the temporary seed's lockfile, never the workspace's.
        let unlocked = tokio::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "--offline",
                "-p",
                "survive-agent",
                "--bin",
                "survive-agent",
            ])
            .arg("--manifest-path")
            .arg(source.join("Cargo.toml"))
            .arg("--target-dir")
            .arg(temp.path().join("build"))
            .output()
            .await
            .unwrap();
        assert!(
            unlocked.status.success(),
            "seed also fails an unlocked offline build:\n{}",
            String::from_utf8_lossy(&unlocked.stderr)
        );
        eprintln!(
            "Seed sources compile offline after allowing lockfile regeneration; the shipped manifest/lockfile pair is incompatible."
        );
    }
    assert!(
        output.status.success(),
        "seed source failed locked build:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn initial_prompt_must_fit_the_default_cores_entire_request_budget() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).unwrap();
    let mut seed = experiment(1);
    seed.initial_prompt = "x".repeat(128 * 1024);
    assert!(
        store.create_experiment("owner", seed).is_err(),
        "accepted immutable 128KiB prompt, but the default core rejects requests above 64KiB before inference and the OCI config supplies no larger context budget"
    );
}
