use serde_json::json;
use survive_core::*;

fn population(seconds: u64) -> (tempfile::TempDir, Store, String) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let env = store
        .create_environment(
            "alice",
            serde_json::from_value(json!({"description":"Explore."})).unwrap(),
        )
        .unwrap();
    let organisms = store
        .birth_organisms(
            "alice",
            &env.id,
            serde_json::from_value(
                json!({"count":1,"defaults":{"initial_energy":100,"lifetime_seconds":seconds}}),
            )
            .unwrap(),
        )
        .unwrap();
    (dir, store, organisms[0].id.clone())
}

#[test]
fn only_observed_running_time_is_charged_and_reboot_closes_the_old_interval() {
    let (_dir, store, id) = population(4000);
    let birth = store.get_agent(&id).unwrap().created_at;
    let observe = |running, boot, mono, wall| {
        store
            .observe_runtime(&id, running, boot, mono, wall)
            .unwrap()
    };
    assert_eq!(
        observe(false, "boot-A", 30000, birth + 30000).remaining_lifetime_ms,
        4_000_000
    );
    observe(true, "boot-A", 30000, birth + 30000);
    let hour = observe(true, "boot-A", 3_630_000, birth + 3_630_000);
    assert_eq!(hour.remaining_lifetime_ms, 400_000);
    assert!(hour.last_heartbeat_at.is_some());
    // Recreating the controller on the same boot continues the observed interval.
    assert_eq!(
        observe(true, "boot-A", 3_640_000, birth + 3_640_000).remaining_lifetime_ms,
        390_000
    );
    // A new host boot must never charge unobserved time from the previous boot.
    assert_eq!(
        observe(false, "boot-B", 1000, birth + 90_000_000).remaining_lifetime_ms,
        390_000
    );
    assert_eq!(
        observe(false, "boot-B", 59000, birth + 91_000_000).remaining_lifetime_ms,
        390_000
    );
    observe(true, "boot-B", 59000, birth + 92_000_000);
    let exhausted = observe(true, "boot-B", 449_000, birth + 92_390_000);
    assert_eq!(exhausted.stop_reasons, vec!["lifetime"]);
    assert!(
        store
            .reserve_priced(
                &id,
                "denied",
                "p",
                1,
                "default",
                EnergyPrice {
                    input: 2,
                    output: 4
                }
            )
            .is_err()
    );
}

#[test]
fn simultaneous_grant_retries_are_atomic_and_restore_both_resources() {
    let (_dir, s, id) = population(2);
    let price = EnergyPrice {
        input: 2,
        output: 5,
    };
    s.reserve_priced(&id, "one", "p", 100, "default", price)
        .unwrap();
    let settled = s
        .settle(
            "one",
            Usage {
                input_tokens: 25,
                output_tokens: 10,
            },
            None,
        )
        .unwrap();
    assert_eq!(settled.energy_charged, Some(100));
    assert_eq!(settled.usage.unwrap().total(), Some(35));
    let wall = now_ms();
    s.observe_runtime(&id, true, "test", 0, wall).unwrap();
    s.observe_runtime(&id, true, "test", 2000, wall + 2000)
        .unwrap();
    s.set_agent_state(&id, AgentState::Depleted, None).unwrap();
    let grant = |k, e, t| {
        serde_json::from_value::<GrantInput>(
            json!({"idempotency_key":k,"energy":e,"lifetime_seconds":t}),
        )
        .unwrap()
    };
    s.grant("alice", &id, grant("energy-only", 50, 0)).unwrap();
    assert_eq!(s.get_agent(&id).unwrap().state, AgentState::Depleted);
    let handles = (0..8)
        .map(|_| {
            let (s, id) = (s.clone(), id.clone());
            std::thread::spawn(move || s.grant("alice", &id, grant("extend", 0, 60)).unwrap().id)
        })
        .collect::<Vec<_>>();
    let unique = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(unique.len(), 1);
    assert_eq!(s.activity_counts(&id).unwrap(), (0, 2));
    let (first, cursor) = s.grant_page(&id, None, 1).unwrap();
    assert_eq!(first[0].input.energy, 50);
    assert_eq!(
        s.grant_page(&id, cursor.as_deref(), 1).unwrap().0[0]
            .input
            .lifetime_seconds,
        60
    );
    assert_eq!(s.get_agent(&id).unwrap().state, AgentState::Queued);
    assert_eq!(
        s.organism_profile(&id).unwrap().remaining_lifetime_ms,
        60000
    );
    // A late stop confirmation must not overwrite the queued wakeup.
    assert_eq!(
        s.set_agent_state(&id, AgentState::Depleted, None)
            .unwrap()
            .state,
        AgentState::Queued
    );
    assert!(s.grant("alice", &id, grant("extend", 0, 61)).is_err());
    assert!(
        s.grant("stranger", &id, grant("unauthorized", 10000, 0))
            .is_err()
    );
}

#[test]
fn retrying_a_report_returns_original_snapshot_and_grants_validate_report_ownership() {
    let (_dir, s, id) = population(120);
    let msg: ReportInput = serde_json::from_value(
        json!({"body":"first","files":["work.txt"],"idempotency_key":"receipt"}),
    )
    .unwrap();
    let original = s
        .save_report(&id, msg.clone(), json!({"work.txt":"old value"}))
        .unwrap();
    let replay = s
        .save_report(&id, msg, json!({"work.txt":"new value"}))
        .unwrap();
    assert_eq!(replay.id, original.id);
    assert_eq!(replay.snapshots, original.snapshots);
    let env = s.organism_profile(&id).unwrap().environment_id;
    let other = s
        .birth_organisms(
            "alice",
            &env,
            serde_json::from_value(
                json!({"defaults":{"initial_energy":10,"lifetime_seconds":5},"count":1}),
            )
            .unwrap(),
        )
        .unwrap();
    let grant: GrantInput = serde_json::from_value(
        json!({"idempotency_key":"credit","report_id":original.id,"energy":1}),
    )
    .unwrap();
    assert!(s.grant("alice", &other[0].id, grant.clone()).is_err());
    assert_eq!(
        s.grant("alice", &id, grant).unwrap().input.report_id,
        Some(original.id)
    );
}

#[test]
fn overriding_a_single_permission_keeps_the_rest_of_the_batch_restrictions() {
    let batch: BirthBatch = serde_json::from_value(json!({
        "defaults": { "initial_energy":10, "lifetime_seconds":5,
                      "network":{"internet":false,"peers":false},
                      "limits":{"memory_bytes":67108864,"cpu_millis":100}},
        "count":1, "overrides":[{"network":{"services":false},"limits":{"pids":15}}]
    }))
    .unwrap();
    let child = &batch.resolve().unwrap()[0];
    assert!(!child.network.internet && !child.network.peers && !child.network.services);
    assert_eq!(
        (
            child.limits.pids,
            child.limits.cpu_millis,
            child.limits.memory_bytes
        ),
        (15, 100, 67108864)
    );
}

#[test]
fn persisted_environment_without_model_policy_inherits_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("db");
    {
        let db = redb::Database::create(&path).unwrap();
        let tx = db.begin_write().unwrap();
        tx.open_table(redb::TableDefinition::<&str, &[u8]>::new("records"))
            .unwrap()
            .insert(
                "env2:legacy",
                &br#"{"id":"legacy","owner":"alice","description":"Explore.","name":"","note":"","created_at":1}"#[..],
            )
            .unwrap();
        tx.commit().unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert!(
        store
            .environment("legacy")
            .unwrap()
            .allowed_models
            .is_none()
    );
    let children = store.birth_organisms("alice", "legacy", serde_json::from_value(json!({
        "count":1,"defaults":{"initial_energy":100,"lifetime_seconds":25,"default_model":"new-alias"}
    })).unwrap()).unwrap();
    store
        .reserve_priced(
            &children[0].id,
            "first",
            "provider",
            1,
            "new-alias",
            EnergyPrice {
                input: 1,
                output: 5,
            },
        )
        .unwrap();
}
