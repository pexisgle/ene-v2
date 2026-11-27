use super::backfill::{MAX_EMBED_BATCH, fill};
use super::index::{Space, dot, encode};
use super::*;
use crate::host::db::memories::MemoryRecord;
use ene_mind::Memory;

fn test_space(model: &str, dimensions: usize) -> String {
    space_id(
        "custom",
        "https://example.invalid/v1",
        "{}",
        model,
        dimensions,
    )
}

#[test]
fn borrowed_cosine_matches_rig_scores_and_ranks() {
    use rig_core::embeddings::{Embedding, distance::VectorDistance};

    for dimensions in [0, 1, 2, 255, 256, 257, 511, 512, 513, 1536] {
        let query: Vec<f32> = (0..dimensions)
            .map(|i| ((i * 17 % 101) as f32 - 50.0) / 13.0)
            .collect();
        let mut space = Space::default();
        let mut expected = Vec::new();
        let embedding = |values: &[f32]| Embedding {
            document: String::new(),
            vec: values.iter().copied().map(f64::from).collect(),
        };
        for candidate in 0..24 {
            let vector: Vec<f32> = (0..dimensions)
                .map(|i| match candidate {
                    0 => 0.0,
                    1 => f32::NAN,
                    2 => f32::INFINITY,
                    3 => f32::MAX,
                    4 => f32::MIN_POSITIVE,
                    _ => ((i * (candidate + 1) % 97) as f32 - 48.0) / 11.0,
                })
                .collect();
            let old = embedding(&query).cosine_similarity(&embedding(&vector), false);
            let new =
                dot(&query, &vector) / (dot(&query, &query).sqrt() * dot(&vector, &vector).sqrt());
            assert!(old.to_bits() == new.to_bits() || (old.is_nan() && new.is_nan()));
            let candidate = RecallCandidate {
                id: candidate.to_string(),
                content_revision: 1,
            };
            space.push(candidate.clone(), 3, &vector, dot(&vector, &vector).sqrt());
            if old.is_finite() {
                expected.push((candidate, old.clamp(-1.0, 1.0)));
            }
        }
        expected.sort_by(|(_, left), (_, right)| right.total_cmp(left));
        expected.truncate(9);
        let mut index = CompactVectorIndex::default();
        index.spaces.insert(("owner".into(), "space".into()), space);
        assert_eq!(
            index.ranked("owner", "space", &query, RecallMode::Auto, 9),
            expected
                .into_iter()
                .map(|(candidate, _)| candidate)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            index.ranked(
                "owner",
                "space",
                &vec![0.0; dimensions + 1],
                RecallMode::Auto,
                9
            ),
            [] as [RecallCandidate; 0]
        );
    }
}

fn spawn_credential_rotation_server(
    listener: tokio::net::TcpListener,
    endpoint_credentials: Credentials,
) -> tokio::task::JoinHandle<Vec<String>> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::spawn(async move {
        let mut headers = Vec::new();
        for count in [1, 1, 1, 1, 1, MAX_EMBED_BATCH, MAX_EMBED_BATCH, 1] {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let mut part = [0; 4096];
                let read = socket.read(&mut part).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&part[..read]);
            }
            let header_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap()
                + 4;
            let content_length: usize = std::str::from_utf8(&request[..header_end])
                .unwrap()
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            while request.len() < header_end + content_length {
                let mut part = [0; 4096];
                let read = socket.read(&mut part).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&part[..read]);
            }
            let request = String::from_utf8(request).unwrap();
            headers.push(
                request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("authorization")
                            .then(|| value.trim().to_owned())
                    })
                    .unwrap_or_default(),
            );
            match headers.len() {
                6 => crate::host::model::save_secret(
                    &endpoint_credentials,
                    "credential-embedding",
                    "backfill-new-key",
                    None,
                )
                .unwrap(),
                7 => {
                    crate::host::model::delete_secret(&endpoint_credentials, "credential-embedding")
                        .unwrap()
                }
                _ => {}
            }
            let body = serde_json::json!({
                "data": (0..count).map(|index| serde_json::json!({
                    "embedding": [1.0, 0.0], "index": index
                })).collect::<Vec<_>>(),
                "usage": {"prompt_tokens": 1, "total_tokens": 1}
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
        headers
    })
}

fn credential_embedding_connection(
    address: std::net::SocketAddr,
) -> crate::host::settings::Connection {
    use crate::host::settings::{Connection, CredentialState, EmbeddingConnection, Endpoint};
    Connection {
        id: ene_domain::ConnectionId::parse("credential-embedding").unwrap(),
        name: "embedding".into(),
        enabled: true,
        kind: ConnectionKind::Embedding,
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
        config: ConnectionConfig::Embedding(EmbeddingConnection {
            endpoint: Endpoint {
                adapter: "custom".into(),
                endpoint: format!("http://{address}"),
                options_schema: String::new(),
                options: String::new(),
            },
            model_id: "credential-model".into(),
            dimensions: 2,
        }),
    }
}

#[tokio::test]
async fn embedding_credentials_rotate_and_revoke_across_clones() {
    use crate::host::{model, settings::CredentialState, tests::Harness};

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let credentials = Credentials::new(true);
    let endpoint = spawn_credential_rotation_server(listener, credentials.clone());
    let harness = Harness::new([]);
    let store = harness.store();
    let writer = credentials.clone();
    let embeddings = Embeddings::default();
    let connection = credential_embedding_connection(address);
    store
        .settings_repo()
        .update_settings(
            store.settings_repo().settings_revision().unwrap(),
            &[(connection, 0)],
        )
        .unwrap();
    let mut companion = store
        .companion_repo()
        .companion(&harness.companion)
        .unwrap()
        .unwrap();
    companion.assignments.embedding = Some(super::super::ModelAssignment {
        connection_id: "credential-embedding".into(),
        model_id: "credential-model".into(),
    });
    store
        .companion_repo()
        .update_companion(&companion, companion.revision)
        .unwrap();
    let revision = store.settings_repo().settings_revision().unwrap();
    for secret in [Some("old-key"), Some("new-key"), None] {
        match secret {
            Some(secret) => {
                model::save_secret(&writer, "credential-embedding", secret, None).unwrap()
            }
            None => model::delete_secret(&writer, "credential-embedding").unwrap(),
        }
        store
            .settings_repo()
            .set_credential_state(
                &ene_domain::ConnectionId::parse("credential-embedding").unwrap(),
                if secret.is_some() {
                    CredentialState::Registered
                } else {
                    CredentialState::Absent
                },
            )
            .unwrap();
        let target = target(&store, Some(&credentials), &companion, &embeddings)
            .unwrap()
            .unwrap();
        assert_eq!(
            request(&target, &["query".into()]).await.unwrap(),
            [vec![1.0, 0.0]]
        );
    }
    writer.remember("credential-embedding", "direct-key".into());
    let target = target(&store, Some(&credentials), &companion, &embeddings)
        .unwrap()
        .unwrap();
    request(&target, &["query".into()]).await.unwrap();
    writer.forget("credential-embedding");
    let target = self::target(&store, Some(&credentials), &companion, &embeddings)
        .unwrap()
        .unwrap();
    request(&target, &["query".into()]).await.unwrap();
    writer.remember("credential-embedding", "backfill-old-key".into());
    let pending = MAX_EMBED_BATCH * 2 + 1;
    for index in 0..pending {
        let mut record = memory(&format!("memory {index}"));
        record.scope = companion.id.clone();
        store.memory_repo().insert_memory(&record).unwrap();
    }
    assert_eq!(
        fill(
            &store.open_secondary().unwrap(),
            &store,
            &embeddings,
            Some(&credentials),
            &companion,
        )
        .await
        .unwrap(),
        pending
    );
    assert_eq!(store.settings_repo().settings_revision().unwrap(), revision);
    let headers = tokio::time::timeout(Duration::from_secs(3), endpoint)
        .await
        .unwrap()
        .unwrap();
    println!("embedding credential lifecycle {headers:?}");
    assert_eq!(
        headers,
        [
            "Bearer old-key",
            "Bearer new-key",
            "Bearer",
            "Bearer direct-key",
            "Bearer",
            "Bearer backfill-old-key",
            "Bearer backfill-new-key",
            "Bearer"
        ]
    );
}

fn store() -> Store {
    let temp = tempfile::tempdir().unwrap();
    Store::open(&temp.path().join("host.sqlite3")).unwrap()
}

fn memory(content: &str) -> MemoryRecord {
    MemoryRecord {
        id: uuid::Uuid::new_v4().to_string(),
        scope: "ene".into(),
        content: content.into(),
        importance: 5,
        occurred_at: None,
        source_context: serde_json::json!({}),
    }
}

fn embed(store: &Store, memory: &Memory, space: &str, vector: &[f32]) {
    assert!(
        store
            .memory_repo()
            .write_embedding(
                &memory.scope,
                &RecallCandidate::from(memory),
                space,
                &encode(vector),
            )
            .unwrap()
    );
}

#[test]
fn the_index_mirrors_only_the_matching_space_and_revision() {
    let store = store();
    let first = store.memory_repo().insert_memory(&memory("tea")).unwrap();
    let second = store
        .memory_repo()
        .insert_memory(&memory("coffee"))
        .unwrap();
    let two = test_space("fake", 2);
    let three = test_space("other", 3);
    embed(&store, &first, &two, &[1.0, 0.0]);
    embed(&store, &second, &three, &[0.0, 0.0, 1.0]);
    let mut index = CompactVectorIndex::default();
    index.sync(&store, "ene", &two).unwrap();
    let ranked = index.ranked("ene", &two, &[1.0, 0.1], RecallMode::Auto, 8);
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].id, first.id);
    assert!(
        index
            .ranked("ene", &two, &[1.0, 0.1, 0.0], RecallMode::Auto, 8)
            .is_empty(),
        "a query of another dimension is not this space"
    );
    index.sync(&store, "ene", &three).unwrap();
    assert_eq!(
        index.ranked("ene", &three, &[0.0, 1.0, 0.0], RecallMode::Auto, 8)[0].id,
        second.id
    );
    assert_eq!(
        index.ranked("ene", &three, &[1.0, 0.0], RecallMode::Auto, 8),
        Vec::<RecallCandidate>::new()
    );

    // A content change invalidates the vector; the next sync drops it.
    store
        .conn()
        .execute(
            "UPDATE memories SET content = 'green tea', \
             content_revision = content_revision + 1, \
             memory_revision = memory_revision + 1 WHERE id = ?1",
            [&first.id],
        )
        .unwrap();
    index.sync(&store, "ene", &two).unwrap();
    assert_eq!(
        index.ranked("ene", &two, &[1.0, 0.1], RecallMode::Auto, 8),
        Vec::<RecallCandidate>::new()
    );

    // A vector of another preprocessing version is a different space.
    let legacy = "fake:2:v0";
    assert!(
        store
            .memory_repo()
            .write_embedding(
                "ene",
                &RecallCandidate::from(&second),
                legacy,
                &encode(&[1.0, 0.0]),
            )
            .is_err()
    );
    index.sync(&store, "ene", legacy).unwrap();
    assert_eq!(
        index.ranked("ene", legacy, &[1.0, 0.0], RecallMode::Auto, 8),
        Vec::<RecallCandidate>::new()
    );
}

#[test]
fn another_companions_vectors_cannot_take_the_companions_top_k() {
    let store = store();
    let space = test_space("fake", 2);
    let mine = store.memory_repo().insert_memory(&memory("mine")).unwrap();
    embed(&store, &mine, &space, &[1.0, 0.0]);
    // Other companions hold equally close vectors for the same query.
    let mut others = Vec::new();
    for index in 0..5 {
        let mut other = memory(&format!("other {index}"));
        other.scope = format!("other-{index}");
        let other = store.memory_repo().insert_memory(&other).unwrap();
        embed(&store, &other, &space, &[1.0, 0.0]);
        others.push(other);
    }
    let mut index = CompactVectorIndex::default();
    index.sync(&store, "ene", &space).unwrap();
    let ranked = index.ranked("ene", &space, &[1.0, 0.0], RecallMode::Auto, 1);
    assert_eq!(ranked.len(), 1);
    assert_eq!(
        ranked[0].id, mine.id,
        "the companion's top-k is not crowded out"
    );
    // Each companion has its own key space: syncing one does not expose
    // another's memories in its ranking.
    index.sync(&store, "other-0", &space).unwrap();
    let theirs = index.ranked("other-0", &space, &[1.0, 0.0], RecallMode::Auto, 10);
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0].id, others[0].id);
    assert!(
        theirs.iter().all(|candidate| candidate.id != mine.id),
        "another companion's vector is never in this companion's space"
    );
}

#[test]
fn ranking_keeps_database_order_for_ties_and_bounds_top_k() {
    let store = store();
    let space = test_space("fake", 2);
    let mut ids = Vec::new();
    for (id, vector) in [
        ("z-first", [2.0, 0.0]),
        ("negative", [-3.0, 0.0]),
        ("a-second", [1.0, 0.0]),
        ("orthogonal", [0.0, 8.0]),
        ("m-third", [20.0, 0.0]),
        ("angled", [3.0, 4.0]),
    ] {
        let mut record = memory(id);
        record.id = id.into();
        let memory = store.memory_repo().insert_memory(&record).unwrap();
        embed(&store, &memory, &space, &vector);
        ids.push(memory.id);
    }
    let mut index = CompactVectorIndex::default();
    index.sync(&store, "ene", &space).unwrap();
    let rank = |limit| {
        index
            .ranked("ene", &space, &[1.0, 0.0], RecallMode::Auto, limit)
            .into_iter()
            .map(|candidate| candidate.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(rank(0), [] as [String; 0]);
    assert_eq!(rank(1), [ids[0].clone()]);
    assert_eq!(rank(2), [ids[0].clone(), ids[2].clone()]);
    assert_eq!(rank(20), [0, 2, 4, 5, 3, 1].map(|i| ids[i].clone()));
    index.sync(&store, "ene", &space).unwrap();
    assert_eq!(
        index.ranked("ene", &space, &[1.0, 0.0], RecallMode::Auto, 2)[1].id,
        ids[2]
    );
}

#[test]
fn invalid_comparisons_do_not_enter_recall() {
    let store = store();
    let space = test_space("fake", 2);
    let mut candidates = Vec::new();
    for vector in [
        [1.0, 0.0],
        [0.0, 0.0],
        [f32::NAN, 0.0],
        [f32::INFINITY, 0.0],
        [-1.0, 0.0],
    ] {
        let memory = store
            .memory_repo()
            .insert_memory(&memory("vector"))
            .unwrap();
        embed(&store, &memory, &space, &vector);
        candidates.push(RecallCandidate::from(&memory));
    }
    let mut index = CompactVectorIndex::default();
    index.sync(&store, "ene", &space).unwrap();
    assert_eq!(
        index.ranked("ene", &space, &[1.0, 0.0], RecallMode::Auto, 20),
        [candidates[0].clone(), candidates[4].clone()]
    );
    for query in [[0.0, 0.0], [f32::NAN, 0.0], [f32::INFINITY, 0.0]] {
        assert_eq!(
            index.ranked("ene", &space, &query, RecallMode::Auto, 2),
            [] as [RecallCandidate; 0]
        );
    }
}

#[test]
fn eligibility_and_precision_are_applied_before_truncation() {
    let store = store();
    let space = test_space("fake", 2);
    let repo = store.memory_repo();
    for _ in 0..20 {
        let mut record = memory("forgotten");
        record.importance = 0;
        let forgotten = repo.insert_memory(&record).unwrap();
        embed(&store, &forgotten, &space, &[1.0, 0.0]);
    }
    let approximate = repo.insert_memory(&memory("approximate")).unwrap();
    let exact = repo.insert_memory(&memory("exact")).unwrap();
    embed(&store, &approximate, &space, &[1.0, 0.0001]);
    embed(&store, &exact, &space, &[1.0, 0.0]);

    let mut index = CompactVectorIndex::default();
    index.sync(&store, "ene", &space).unwrap();
    let query = [1.0, 0.0];
    assert_eq!(
        index.ranked("ene", &space, &query, RecallMode::Auto, 20),
        [
            RecallCandidate::from(&exact),
            RecallCandidate::from(&approximate)
        ]
    );
    assert_eq!(
        index
            .ranked("ene", &space, &query, RecallMode::Explicit, 20)
            .len(),
        20
    );
    repo.control_memory(&exact.id, ene_mind::MemoryActionKind::Forget, "ene")
        .unwrap();
    index.sync(&store, "ene", &space).unwrap();
    assert_eq!(
        index.ranked("ene", &space, &query, RecallMode::Auto, 20),
        [RecallCandidate::from(&approximate)]
    );
}

#[test]
fn q03_space_id_distinguishes_endpoints_adapters_and_options_without_secrets() {
    let base = space_id(
        "openai",
        "https://api.example/v1/",
        r#"{"b":2,"a":1}"#,
        "nomic-embed:latest",
        768,
    );
    let reordered = space_id(
        "openai",
        "https://api.example/v1",
        r#"{ "a": 1, "b": 2 }"#,
        "nomic-embed:latest",
        768,
    );
    assert_eq!(base, reordered);
    assert_eq!(dimensions_of_space(&base), Some(768));

    let diff_endpoint = space_id(
        "openai",
        "https://other.example/v1",
        r#"{"a":1,"b":2}"#,
        "nomic-embed:latest",
        768,
    );
    let diff_adapter = space_id(
        "gemini",
        "https://api.example/v1",
        r#"{"a":1,"b":2}"#,
        "nomic-embed:latest",
        768,
    );
    let diff_options = space_id(
        "openai",
        "https://api.example/v1",
        r#"{"a":1,"b":3}"#,
        "nomic-embed:latest",
        768,
    );
    assert_ne!(base, diff_endpoint);
    assert_ne!(base, diff_adapter);
    assert_ne!(base, diff_options);
}

async fn start_fake_embed_server(val: f32) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = socket.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let body = serde_json::json!({
                "data": [{"embedding": [val, 0.0], "index": 0}],
                "usage": {"prompt_tokens": 1, "total_tokens": 1}
            })
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _discarded = socket.write_all(reply.as_bytes()).await;
        }
    });
    format!("http://{addr}")
}

fn custom_embedding_conn(
    id: &str,
    name: &str,
    endpoint: String,
    options: &str,
) -> crate::host::settings::Connection {
    use crate::host::settings::{Connection, CredentialState, EmbeddingConnection, Endpoint};
    Connection {
        id: ene_domain::ConnectionId::parse(id).unwrap(),
        name: name.into(),
        enabled: true,
        kind: ConnectionKind::Embedding,
        credential_state: CredentialState::Absent,
        revision: 0,
        last_check: None,
        config: ConnectionConfig::Embedding(EmbeddingConnection {
            endpoint: Endpoint {
                adapter: "custom".into(),
                endpoint,
                options_schema: String::new(),
                options: options.into(),
            },
            model_id: "shared-name".into(),
            dimensions: 2,
        }),
    }
}

#[tokio::test]
async fn q03_backfill_scopes_by_companion_and_prevents_vector_overwrites() {
    use crate::host::tests::Harness;

    let endpoint_a = start_fake_embed_server(1.0).await;
    let endpoint_b = start_fake_embed_server(0.0).await;
    let harness = Harness::new([]);
    let store = harness.store();
    let conn_a = custom_embedding_conn("emb-alpha", "Alpha", endpoint_a, r#"{"profile":"alpha"}"#);
    let conn_b = custom_embedding_conn("emb-beta", "Beta", endpoint_b, r#"{"profile":"beta"}"#);
    store
        .settings_repo()
        .update_settings(
            store.settings_repo().settings_revision().unwrap(),
            &[(conn_a, 0), (conn_b, 0)],
        )
        .unwrap();

    let mut comp_a = store
        .companion_repo()
        .companion(&harness.companion)
        .unwrap()
        .unwrap();
    comp_a.assignments.embedding = Some(super::super::ModelAssignment {
        connection_id: "emb-alpha".into(),
        model_id: "shared-name".into(),
    });
    store
        .companion_repo()
        .update_companion(&comp_a, comp_a.revision)
        .unwrap();

    let mut comp_b = comp_a.clone();
    comp_b.id = "beta".into();
    comp_b.name = "Beta".into();
    comp_b.assignments.embedding = Some(super::super::ModelAssignment {
        connection_id: "emb-beta".into(),
        model_id: "shared-name".into(),
    });
    store.companion_repo().insert_companion(&comp_b).unwrap();

    let mut mem_a = memory("alpha private memory");
    mem_a.scope = comp_a.id.clone();
    let mem_a = store.memory_repo().insert_memory(&mem_a).unwrap();

    let mut mem_b = memory("beta private memory");
    mem_b.scope = comp_b.id.clone();
    let mem_b = store.memory_repo().insert_memory(&mem_b).unwrap();

    let embeddings = Embeddings::default();
    let target_a = target(&store, None, &comp_a, &embeddings).unwrap().unwrap();
    let target_b = target(&store, None, &comp_b, &embeddings).unwrap().unwrap();
    assert_ne!(target_a.space, target_b.space);

    let missing_a = store
        .memory_repo()
        .memories_missing_embedding(&comp_a.id, &target_a.space, 10)
        .unwrap();
    assert_eq!(
        missing_a
            .iter()
            .map(|m| m.candidate.id.as_str())
            .collect::<Vec<_>>(),
        [mem_a.id.as_str()]
    );

    fill(
        &store.open_secondary().unwrap(),
        &store,
        &embeddings,
        None,
        &comp_a,
    )
    .await
    .unwrap();
    assert!(
        store
            .memory_repo()
            .embedding_blob(&RecallCandidate::from(&mem_a), &target_a.space)
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .memory_repo()
            .embedding_blob(&RecallCandidate::from(&mem_b), &target_a.space)
            .unwrap()
            .is_none()
    );

    fill(
        &store.open_secondary().unwrap(),
        &store,
        &embeddings,
        None,
        &comp_b,
    )
    .await
    .unwrap();
    assert!(
        store
            .memory_repo()
            .embedding_blob(&RecallCandidate::from(&mem_b), &target_b.space)
            .unwrap()
            .is_some()
    );

    // Alpha's space was not polluted by Beta's backfill.
    let mut index = CompactVectorIndex::default();
    index.sync(&store, &comp_a.id, &target_a.space).unwrap();
    let a_ranked = index.ranked(
        &comp_a.id,
        &target_a.space,
        &[1.0, 0.0],
        RecallMode::Auto,
        10,
    );
    assert_eq!(a_ranked.len(), 1);
    assert_eq!(a_ranked[0].id, mem_a.id);
}

#[tokio::test]
async fn sakura_embedding_omits_dimensions_and_decodes_without_usage() {
    use crate::host::settings::{
        Connection, ConnectionConfig, ConnectionKind, CredentialState, EmbeddingConnection,
        Endpoint,
    };
    use crate::host::tests::Harness;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = socket.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&buf[..pos]);
                    let cl = header
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if buf.len() >= pos + 4 + cl {
                        let body_start = pos + 4;
                        let _discarded = tx.send(
                            String::from_utf8_lossy(&buf[body_start..body_start + cl]).into_owned(),
                        );
                        break;
                    }
                }
            }
            let vec1024 = vec![0.25f64; 1024];
            let body = serde_json::json!({
                "model": "multilingual-e5-large",
                "data": [{
                    "index": 0,
                    "object": "embedding",
                    "embedding": vec1024,
                }]
            })
            .to_string();
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
    });

    let credentials = Credentials::new(true);
    credentials.remember("sakura-emb", "uuid:secret".into());
    let connection = Connection {
        id: ene_domain::ConnectionId::parse("sakura-emb").unwrap(),
        name: "Sakura Embedding".into(),
        enabled: true,
        kind: ConnectionKind::Embedding,
        credential_state: CredentialState::Registered,
        revision: 1,
        last_check: None,
        config: ConnectionConfig::Embedding(EmbeddingConnection {
            endpoint: Endpoint {
                adapter: "sakura".into(),
                endpoint: format!("http://{addr}/v1"),
                options_schema: String::new(),
                options: "{}".into(),
            },
            model_id: "multilingual-e5-large".into(),
            dimensions: 0,
        }),
    };
    let providers = crate::host::providers::Providers::none();
    let actual_dim = probe_dimensions(&connection, &credentials, &providers)
        .await
        .unwrap();
    assert_eq!(actual_dim, 1024);

    let harness = Harness::new([]);
    let store = harness.store();
    let rev = store.settings_repo().settings_revision().unwrap();
    store
        .settings_repo()
        .update_settings(rev, &[(connection, 0)])
        .unwrap();
    let mut comp = store
        .companion_repo()
        .companion(&harness.companion)
        .unwrap()
        .unwrap();
    comp.assignments.embedding = Some(super::super::ModelAssignment {
        connection_id: "sakura-emb".into(),
        model_id: "multilingual-e5-large".into(),
    });
    let embeddings = Embeddings::default();
    let resolved = target(&store, Some(&credentials), &comp, &embeddings)
        .unwrap()
        .unwrap();
    assert_eq!(resolved.dimensions, 1024);
    let vectors = request(&resolved, &["こんにちは".into()]).await.unwrap();
    assert_eq!(vectors.len(), 1);
    assert_eq!(vectors[0].len(), 1024);

    for _ in 0..2 {
        let body_str = rx.recv().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body_str).unwrap();
        assert_eq!(parsed["model"], "multilingual-e5-large");
        assert!(
            parsed.get("dimensions").is_none(),
            "sakura dialect must omit OpenAI dimensions field: {parsed}"
        );
    }
}
