use super::*;
#[test]
fn environment_payload_preserves_template_edits_and_omits_blank_metadata() {
    let mut d = Draft::new(Action::Environment);
    assert!(d.payload().is_err());
    d.fields["description"] = json!("自由探索，报告观察结果。");
    let input: survive_core::EnvironmentInput =
        serde_json::from_value(d.payload().unwrap()).unwrap();
    assert_eq!(input.description, "自由探索，报告观察结果。");
    assert!(input.name.is_empty());
    assert!(input.note.is_empty());
}
#[test]
fn batch_form_defaults_and_overrides_resolve_through_core() {
    let mut d = Draft::new(Action::Birth("garden".into()));
    d.fields["count"] = json!("2");
    d.fields["default_model"] = json!("gpt-4.1-mini");
    d.fields["allowed_models"] = json!(["gpt-4.1-mini", "long"]);
    d.fields["overrides"] = json!({"2":{"dna_mode":"custom","dna":"act","default_model":"long","lifetime_seconds":"30","initial_energy":"800","internet":"false","pids":"32"}});
    let batch: survive_core::BirthBatch = serde_json::from_value(d.payload().unwrap()).unwrap();
    let births = batch.resolve().unwrap();
    assert_eq!(
        json!(births[0].limits),
        json!(survive_core::ResourceLimits::default())
    );
    assert_eq!(births[0].initial_energy, 50000);
    assert_eq!(births[1].initial_energy, 800);
    assert_eq!(births[1].lifetime_seconds, 30);
    assert_eq!(births[1].default_model, "long");
    assert_eq!(births[1].dna, "act");
    assert!(!births[1].network.internet);
    assert_eq!(births[1].limits.pids, 32);
    assert_eq!(births[1].limits.memory_bytes, births[0].limits.memory_bytes);
    d.fields["allowed_models"] = Value::Null;
    d.fields["overrides"] = json!({});
    assert!(
        d.payload().unwrap()["defaults"]
            .get("allowed_models")
            .is_none()
    );
}
#[test]
fn grant_requires_value_and_preserves_optional_report_and_retry_key() {
    let mut d = Draft {
        action: Action::Grant("life".into()),
        fields: json!({"idempotency_key":"grant-12"}),
        submitting: false,
        error: String::new(),
    };
    assert!(d.payload().is_err());
    d.fields["lifetime_seconds"] = json!("30");
    let direct: survive_core::GrantInput = serde_json::from_value(d.payload().unwrap()).unwrap();
    assert_eq!(direct.energy, 0);
    assert_eq!(direct.report_id, None);
    d.fields["energy"] = json!("400");
    d.fields["report_id"] = json!("report-12");
    let associated: survive_core::GrantInput =
        serde_json::from_value(d.payload().unwrap()).unwrap();
    assert_eq!(associated.idempotency_key, direct.idempotency_key);
    assert_eq!(associated.report_id.as_deref(), Some("report-12"));
}

#[test]
fn creator_workflow_persists_form_payloads_and_retries_one_grant() {
    let dir = tempfile::tempdir().unwrap();
    let db = survive_core::Store::open(dir.path().join("life.redb")).unwrap();
    let owner = "observatory";
    let mut environment = Draft::new(Action::Environment);
    environment.fields["description"] = json!(forms::OPEN_ENVIRONMENT);
    let env = db
        .create_environment(
            owner,
            serde_json::from_value(environment.payload().unwrap()).unwrap(),
        )
        .unwrap();
    assert!(db.profiles(Some(owner)).unwrap().is_empty());
    let birth = Draft::new(Action::Birth(env.id.clone()));
    let roster = db
        .birth_organisms(
            owner,
            &env.id,
            serde_json::from_value(birth.payload().unwrap()).unwrap(),
        )
        .unwrap();
    let recipient = &roster[0].id;
    let report = db
        .save_report(
            recipient,
            survive_core::ReportInput {
                idempotency_key: "report-1".into(),
                body: "Results".into(),
                files: vec![],
            },
            json!([]),
        )
        .unwrap();
    let draft = Draft {
        action: Action::Grant(recipient.clone()),
        fields: json!({"energy":"50","lifetime_seconds":"20","report_id":report.id,"idempotency_key":"grant-1"}),
        error: String::new(),
        submitting: false,
    };
    let before = db.life_state(recipient).unwrap();
    for _ in 0..2 {
        db.grant(
            owner,
            recipient,
            serde_json::from_value(draft.payload().unwrap()).unwrap(),
        )
        .unwrap();
    }
    let after = db.life_state(recipient).unwrap();
    assert_eq!(after.energy_balance, before.energy_balance + 50);
    assert_eq!(
        after.remaining_lifetime_ms,
        before.remaining_lifetime_ms + 20000
    );
    assert_eq!(db.grants(recipient).unwrap().len(), 1);
}

#[test]
fn environment_and_slot_model_policy_preserve_inheritance_and_validate_choices() {
    let mut env = Draft::new(Action::Environment);
    env.fields["description"] = json!("test");
    env.fields["allowed_models"] = json!(["small", "large"]);
    assert_eq!(
        env.payload().unwrap()["allowed_models"],
        json!(["small", "large"])
    );
    let mut birth = Draft::new(Action::Birth("garden".into()));
    birth.fields["default_model"] = json!("small");
    birth.fields["overrides"] = json!({"1":{"allowed_models":["large"]}});
    assert_eq!(
        birth.payload().unwrap()["overrides"][0]["default_model"],
        json!("large")
    );
    birth.fields["overrides"]["1"]["default_model"] = json!("small");
    assert!(birth.payload().unwrap_err().contains("第 1 个生物"));
    birth.fields["overrides"]["1"]["default_model"] = json!("large");
    assert_eq!(
        birth.payload().unwrap()["overrides"][0]["allowed_models"],
        json!(["large"])
    );
    birth.fields["allowed_models"] = json!([]);
    assert!(birth.payload().is_err());
}
