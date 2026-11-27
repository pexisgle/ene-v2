use super::*;

#[test]
fn operations_are_idempotent_and_bound_to_one_client() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
    let repo = store.client_repo();
    let operation = DeviceOperation {
        id: "op1".into(),
        client_id: "c1".into(),
        capability: "body.motion".into(),
        input: serde_json::json!({"motion": "nod"}),
        scope: DeviceScope::Companion {
            companion_id: CompanionId::parse("ene").unwrap(),
            activation_generation: Some(1),
        },
        status: DeviceStatus::Pending,
        output: None,
        failure: None,
        created_at: String::new(),
        updated_at: String::new(),
    };
    repo.insert_operation(&operation).unwrap();
    assert_eq!(repo.outstanding_operations("c1").unwrap().len(), 1);
    repo.set_operation_status("op1", DeviceStatus::Dispatched)
        .unwrap();
    let completed = repo
        .complete_operation("op1", Ok(&serde_json::json!({"ok": true})))
        .unwrap()
        .unwrap();
    assert_eq!(completed.status, DeviceStatus::Completed);
    let again = repo
        .complete_operation("op1", Err(&Failure::new("operation_failed", "late report")))
        .unwrap()
        .unwrap();
    assert_eq!(again.status, DeviceStatus::Completed);
    repo.set_operation_status("op1", DeviceStatus::Uncertain)
        .unwrap();
    let saved = repo.operation("op1").unwrap().unwrap();
    assert_eq!(saved.client_id, "c1");
    assert_eq!(saved.status, DeviceStatus::Completed);
    assert_eq!(saved.output, completed.output);
    let other = DeviceOperation {
        id: "op2".into(),
        client_id: "c2".into(),
        ..operation.clone()
    };
    repo.insert_operation(&other).unwrap();
    assert_eq!(repo.outstanding_operations("c1").unwrap().len(), 0);
    assert_eq!(repo.outstanding_operations("c2").unwrap().len(), 1);
}
