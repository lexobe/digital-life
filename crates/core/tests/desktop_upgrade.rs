use serde_json::json;
use survive_core::*;

fn fixture() -> (tempfile::TempDir, Store, Agent) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("platform.redb")).unwrap();
    let env = store
        .create_environment(
            "creator",
            serde_json::from_value(json!({"description":"Home."})).unwrap(),
        )
        .unwrap();
    let agent = store.birth_organisms("creator", &env.id, serde_json::from_value(json!({
        "count":1, "defaults":{"initial_energy":100,"lifetime_seconds":60,
            "network":{"internet":false},"limits":{"memory_bytes":67108864,"pids":64,"cpu_millis":500}}
    })).unwrap()).unwrap().remove(0);
    store
        .set_agent_state(&agent.id, AgentState::Failed, Some("stopped".into()))
        .unwrap();
    (dir, store, agent)
}

fn finalize(store: &Store, agent: &Agent, limits: &ResourceLimits) -> anyhow::Result<AgentState> {
    store.upgrade_desktop_limits(
        &agent.id,
        &agent.experiment_id,
        agent.slot,
        &agent.token_hash,
        &store.organism_profile(&agent.id)?.birth.network,
        limits,
    )
}

#[test]
fn migration_requires_matching_private_spec_and_paused_clock_and_audits_exactly_once() {
    let (dir, store, agent) = fixture();
    let id = &agent.id;
    let profile = store.organism_profile(id).unwrap();
    let limits = ResourceLimits::default();
    let apply = |environment: &str, slot, hash: &str, network: &NetworkPolicy| {
        store.upgrade_desktop_limits(id, environment, slot, hash, network, &limits)
    };
    let network = &profile.birth.network;
    assert!(apply("wrong", agent.slot, &agent.token_hash, network).is_err());
    assert!(
        apply(
            &agent.experiment_id,
            agent.slot + 1,
            &agent.token_hash,
            network
        )
        .is_err()
    );
    assert!(apply(&agent.experiment_id, agent.slot, "other-hash", network).is_err());
    assert!(
        apply(
            &agent.experiment_id,
            agent.slot,
            &agent.token_hash,
            &NetworkPolicy::default()
        )
        .is_err()
    );
    store
        .observe_runtime(id, true, "boot", 1000, now_ms())
        .unwrap();
    assert!(finalize(&store, &agent, &limits).is_err());
    assert!(store.organism_profile(id).unwrap().runtime_limits.is_none());
    assert_eq!(store.get_agent(id).unwrap().state, AgentState::Failed);
    assert!(
        !store
            .events(id, None, 100)
            .unwrap()
            .iter()
            .any(|e| e.kind == "desktop_upgrade")
    );
    store
        .observe_runtime(id, false, "boot", 1000, now_ms())
        .unwrap();

    assert_eq!(
        finalize(&store, &agent, &limits).unwrap(),
        AgentState::Queued
    );
    drop(store);
    let store = Store::open(dir.path().join("platform.redb")).unwrap();
    assert_eq!(
        finalize(&store, &agent, &limits).unwrap(),
        AgentState::Queued
    );
    let events = store
        .events(id, None, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "desktop_upgrade")
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["old_state"], "failed");
    assert_eq!(events[0].data["state"], "queued");
    assert!(!events[0].data.to_string().contains(&agent.token_hash));
    let updated = store.organism_profile(id).unwrap();
    assert_eq!(updated.birth.limits, profile.birth.limits);
    assert_eq!(updated.effective_limits(), &limits);
    assert_eq!(updated.remaining_lifetime_ms, profile.remaining_lifetime_ms);
    let updated = store.get_agent(id).unwrap();
    assert_eq!(
        (updated.generation, updated.balance()),
        (agent.generation, agent.balance())
    );
    assert!(updated.last_error.is_none());
}

#[test]
fn migration_keeps_individuals_with_exhausted_resources_depleted() {
    for exhaust_energy in [true, false] {
        let (_dir, store, agent) = fixture();
        let id = &agent.id;
        if exhaust_energy {
            store
                .reserve_priced(
                    id,
                    "final-turn",
                    "fixture",
                    100,
                    "default",
                    EnergyPrice {
                        input: 1,
                        output: 1,
                    },
                )
                .unwrap();
            store
                .settle(
                    "final-turn",
                    Usage {
                        input_tokens: 99,
                        output_tokens: 1,
                    },
                    None,
                )
                .unwrap();
        } else {
            store
                .observe_runtime(id, true, "boot", 0, now_ms())
                .unwrap();
            store
                .observe_runtime(id, true, "boot", 60_000, now_ms())
                .unwrap();
            store
                .observe_runtime(id, false, "boot", 60_000, now_ms())
                .unwrap();
        }
        let before = store.life_state(id).unwrap();
        assert_eq!(
            finalize(&store, &agent, &ResourceLimits::default()).unwrap(),
            AgentState::Depleted
        );
        let after = store.life_state(id).unwrap();
        assert_eq!(before.energy_balance, after.energy_balance);
        assert_eq!(before.remaining_lifetime_ms, after.remaining_lifetime_ms);
        assert!(store.grants(id).unwrap().is_empty());
    }
}
