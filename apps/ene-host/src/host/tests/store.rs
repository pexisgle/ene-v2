use super::*;
#[tokio::test]
async fn an_unknown_target_never_spawns_an_agent() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = Runtime::start_with(
        temp.path(),
        scripted(text_model("ok"), text_model("unused"), false),
    )
    .await
    .unwrap();
    let missing = runtime
        .read(|app| {
            chat::history(
                app.world_mut(),
                w::HistoryRequest {
                    companion_id: "missing".into(),
                    ..Default::default()
                },
            )
        })
        .await;
    assert!(matches!(missing, Err(HostError::NotFound(_))));
    let agents = runtime
        .call(|app| {
            let mut query = app.world_mut().query::<&AgentKey>();
            Ok(query.iter(app.world()).count())
        })
        .await
        .unwrap();
    assert_eq!(agents, 1);
    runtime.shutdown().await.unwrap();
}

#[test]
fn a_foreign_database_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("host.sqlite3");
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute_batch("CREATE TABLE documents (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
        .unwrap();
    drop(connection);
    assert!(Store::open(&path).is_err());
}

#[tokio::test]
async fn companion_seed_is_persistent() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
    let companions = store.companions().unwrap();
    assert_eq!(companions.len(), 1);
    assert_eq!(companions[0].name, "ene");
    drop(store);
    let store = Store::open(&temp.path().join("host.sqlite3")).unwrap();
    assert_eq!(store.companions().unwrap().len(), 1);
}
