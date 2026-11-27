use super::tests::{
    companion_replied_with, configure_test_search, search_test_connection, spawn_searx_test_server,
};
use super::*;
use crate::host::{permission, settings::CredentialState, tests::Harness};
use ene_policy::{PolicyEffect, PolicyRule, Verdict, decide_action};
use rig_core::test_utils::MockStreamEvent;
use std::sync::atomic::{AtomicUsize, Ordering};

fn test_credentials(secret: &str) -> Credentials {
    let mut credentials = Credentials::new(true);
    credentials.approval_key = Some([17; 32]);
    credentials.remember("exa", secret.into());
    credentials
}

#[test]
fn only_keyed_searches_consult_the_credential_store() {
    let credentials = Credentials::new(false);
    credentials.make_unreadable();
    let providers = Providers::none();
    let mut connection = search_test_connection(
        "search",
        "Private SearXNG",
        "searxng",
        "http://127.0.0.1:8080/search".into(),
        CredentialState::Absent,
        5,
    );
    let result = resolve_connection(&connection, Some(&credentials), &providers, 0).unwrap();
    assert!(result.binding.secret.is_none());
    connection = search_test_connection(
        "search",
        "Exa",
        "exa",
        "https://search.example/".into(),
        CredentialState::Registered,
        5,
    );
    assert!(matches!(
        resolve_connection(&connection, Some(&credentials), &providers, 0),
        Err(SearchConfigError::Credential(_))
    ));
}

#[test]
fn search_rejects_unsafe_routes_before_an_explicit_probe_or_model_call() {
    for url in [
        "http://169.254.169.254/?option=private",
        "http://[::ffff:169.254.169.254]/?option=private",
        "https://metadata.google.internal./?option=private",
    ] {
        let error = search_endpoint(SearchAdapter::Searxng, url).unwrap_err();
        assert!(matches!(error, SearchConfigError::BlockedDestination));
        assert!(!error.to_string().contains("option=private"));
    }
    assert!(matches!(
        search_endpoint(SearchAdapter::Exa, "http://search.example/lookup"),
        Err(SearchConfigError::InsecureCredentialRoute)
    ));
    assert!(search_endpoint(SearchAdapter::Exa, "http://[::1]:8000/lookup").is_ok());
}

#[test]
fn persisted_search_rules_follow_effective_requests_and_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("host.sqlite3");
    let store = Store::open(&path).unwrap();
    let exa = search_test_connection(
        "exa",
        "Research account",
        "exa",
        "https://search.example/lookup?private=tenant-a".into(),
        CredentialState::Registered,
        3,
    );
    let providers = Providers::none();
    let credentials = test_credentials("original-credential");
    let repo = store.settings_repo();
    repo.update_settings(repo.settings_revision().unwrap(), &[(exa.clone(), 0)])
        .unwrap();
    let request = json!({"query": " ene ", "limit": 7});
    let action = |store: &Store, credentials: &Credentials, request: &Value| {
        resolve(store, Some(credentials), &providers)
            .unwrap()
            .prepare(request, Some(credentials))
            .unwrap()
            .candidate()
    };
    let CandidateAction::WebSearch(prepared) = action(&store, &credentials, &request) else {
        panic!("expected a search action");
    };
    let saved = prepared.scope.as_ref().unwrap().allow_rule();
    assert_eq!(
        saved.scope_label(),
        Some("Exa · Research account · \"ene\" · 3 · https://search.example/lookup")
    );
    assert!(matches!(
        resolve(&store, Some(&credentials), &providers)
            .unwrap()
            .prepare(
                &json!({"query": "send original-credential"}),
                Some(&credentials)
            ),
        Err(SearchInputError::CredentialInQuery)
    ));
    assert!(!saved.pattern.contains("tenant-a"));
    assert!(!saved.pattern.contains("original-credential"));
    store
        .policy_repo()
        .apply_policy_rules(&[saved], &[])
        .unwrap();
    drop(store);

    let store = Store::open(&path).unwrap();
    let rules = store.policy_repo().policy_rules().unwrap();
    let credentials = test_credentials("original-credential");
    credentials.remember("unrelated", "different key".into());
    let applies = |credentials: &Credentials, request: &Value| {
        decide_action(&rules, &action(&store, credentials, request))
    };
    assert_eq!(
        applies(&credentials, &json!({"query": "ene"})),
        Verdict::Allow
    );
    let mut confirmation = rules[0].clone();
    confirmation.effect = ene_policy::PolicyEffect::Confirm;
    assert!(matches!(
        decide_action(
            &[confirmation],
            &action(&store, &credentials, &json!({"query": "ene"}))
        ),
        Verdict::HumanRequired { .. }
    ));
    for difference in [
        applies(&credentials, &json!({"query": "ene", "limit": 2})),
        applies(&credentials, &json!({"query": "different"})),
        applies(&test_credentials("replacement-credential"), &request),
    ] {
        assert!(matches!(difference, Verdict::NeedsDecision { .. }));
    }
    let repo = store.settings_repo();
    let revision = repo.connection(exa.id.as_str()).unwrap().unwrap().revision;
    repo.delete_connection(&exa.id, revision, || Ok::<_, ene_store::StoreError>(()))
        .unwrap();
    repo.update_settings(repo.settings_revision().unwrap(), &[(exa, 0)])
        .unwrap();
    assert!(matches!(
        applies(&credentials, &request),
        Verdict::NeedsDecision { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deny_saved_after_approval_prevents_the_pending_search_request() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (address, server) = spawn_searx_test_server(Arc::clone(&hits)).await;
    let mut harness = Harness::new([
        vec![
            MockStreamEvent::tool_call("search", NAME, json!({"query": "ene"})),
            MockStreamEvent::final_response_with_default_usage(),
        ],
        vec![
            MockStreamEvent::text("送信が中止されました"),
            MockStreamEvent::final_response_with_default_usage(),
        ],
    ]);
    configure_test_search(&harness, format!("http://{address}/searx"));
    let store = harness.store();
    let policies = store.policy_repo();
    let policy = |effect| PolicyRule {
        id: "search-permission".into(),
        tool_name: NAME.into(),
        pattern: "*".into(),
        effect,
        created_at: String::new(),
    };
    policies
        .apply_policy_rules(&[policy(PolicyEffect::Confirm)], &[])
        .unwrap();
    harness.accept("search-request", "調べて").unwrap();
    let pending = || {
        store
            .approval_repo()
            .approvals(permission::ApprovalStatus::Pending)
            .unwrap()
    };
    harness.run_until(|_| !pending().is_empty());
    permission::submit_with_outcome(
        harness.app.world_mut(),
        permission::ApprovalRequest::Operator(&pending()[0].request_id),
        permission::ApprovalDecision::Approve,
    )
    .unwrap();
    policies
        .apply_policy_rules(&[policy(PolicyEffect::Deny)], &[])
        .unwrap();
    harness.run_until(|world| companion_replied_with(world, "送信が中止されました"));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn always_allow_saves_one_rule_while_confirm_still_controls_dispatch() {
    let hits = Arc::new(AtomicUsize::new(0));
    let (address, server) = spawn_searx_test_server(Arc::clone(&hits)).await;
    let mut harness = Harness::new((0..3).flat_map(|index| {
        [
            vec![
                MockStreamEvent::tool_call(format!("call-{index}"), NAME, json!({"query": "ene"})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            vec![
                MockStreamEvent::text(format!("検索完了 {index}")),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]
    }));
    harness
        .app
        .world_mut()
        .resource_mut::<Credentials>()
        .approval_key = Some([17; 32]);
    configure_test_search(&harness, format!("http://{address}/searx"));
    let store = harness.store();
    let confirm = PolicyRule {
        id: "confirm-search".into(),
        tool_name: NAME.into(),
        pattern: "*".into(),
        effect: PolicyEffect::Confirm,
        created_at: String::new(),
    };
    let repo = store.policy_repo();
    repo.apply_policy_rules(std::slice::from_ref(&confirm), &[])
        .unwrap();
    let pending = || {
        store
            .approval_repo()
            .approvals(permission::ApprovalStatus::Pending)
            .unwrap()
    };
    for index in 0..3 {
        if index == 2 {
            repo.apply_policy_rules(&[], std::slice::from_ref(&confirm.id))
                .unwrap();
        }
        harness.accept(&format!("input-{index}"), "調べて").unwrap();
        if index < 2 {
            harness.run_until(|_| !pending().is_empty());
            assert_eq!(hits.load(Ordering::SeqCst), index);
            let outcome = permission::submit_with_outcome(
                harness.app.world_mut(),
                permission::ApprovalRequest::Operator(&pending()[0].request_id),
                permission::ApprovalDecision::AlwaysApprove,
            )
            .unwrap();
            assert!(matches!(outcome.rule, permission::RuleSave::Saved));
            let rules = repo.policy_rules().unwrap();
            assert_eq!(
                rules
                    .iter()
                    .filter(|rule| rule.scope_label().is_some())
                    .count(),
                1
            );
        }
        harness.run_until(|world| companion_replied_with(world, &format!("検索完了 {index}")));
        assert_eq!(hits.load(Ordering::SeqCst), index + 1);
        assert_eq!(pending(), [] as [ene_policy::Approval; 0]);
    }
    server.abort();
}

#[test]
fn search_requires_a_single_explicit_destination_even_if_another_is_ready() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("host.sqlite3")).unwrap();
    let creds = Credentials::new(true);
    creds.remember("search-exa", "exa-valid-key".into());
    let providers = Providers::none();
    let repo = store.settings_repo();
    let mut unconfigured = search_test_connection(
        "search-tavily",
        "Tavily",
        "tavily",
        String::new(),
        CredentialState::Absent,
        5,
    );
    let mut ready = search_test_connection(
        "search-exa",
        "Exa",
        "exa",
        String::new(),
        CredentialState::Registered,
        5,
    );
    repo.update_settings(
        repo.settings_revision().unwrap(),
        &[(unconfigured.clone(), 0), (ready.clone(), 0)],
    )
    .unwrap();
    assert!(matches!(
        resolve(&store, Some(&creds), &providers),
        Err(SearchConfigError::AmbiguousSelection)
    ));

    let revision_of = |id: &str| repo.connection(id).unwrap().unwrap().revision;
    ready.enabled = false;
    let revision = revision_of(ready.id.as_str());
    repo.update_settings(
        repo.settings_revision().unwrap(),
        &[(ready.clone(), revision)],
    )
    .unwrap();
    assert!(matches!(
        resolve(&store, Some(&creds), &providers),
        Err(SearchConfigError::MissingCredential("Tavily"))
    ));

    unconfigured.enabled = false;
    ready.enabled = true;
    let unconfigured_revision = revision_of(unconfigured.id.as_str());
    let ready_revision = revision_of(ready.id.as_str());
    repo.update_settings(
        repo.settings_revision().unwrap(),
        &[
            (unconfigured, unconfigured_revision),
            (ready, ready_revision),
        ],
    )
    .unwrap();
    let resolved = resolve(&store, Some(&creds), &providers).unwrap();
    assert_eq!(resolved.binding.adapter, SearchAdapter::Exa);
    assert_eq!(
        resolved.binding.endpoint.as_str(),
        SearchAdapter::Exa.provider().default_endpoint.unwrap()
    );
}

#[test]
fn held_search_pins_the_current_connection_and_effective_credential() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("host.sqlite3")).unwrap();
    let repo = store.settings_repo();
    let creds = Credentials::new(true);
    let providers = Providers::none();
    let exa = search_test_connection(
        "exa",
        "Exa",
        "exa",
        "https://exa.example/search".into(),
        CredentialState::Registered,
        5,
    );
    let mut other = search_test_connection(
        "other",
        "Mwmbl",
        "mwmbl",
        "https://other.example/".into(),
        CredentialState::Absent,
        5,
    );
    other.enabled = false;
    creds.remember("exa", "original".into());
    repo.update_settings(repo.settings_revision().unwrap(), &[(exa.clone(), 0)])
        .unwrap();
    let original = resolve(&store, Some(&creds), &providers).unwrap().binding;

    repo.update_settings(repo.settings_revision().unwrap(), &[(other, 0)])
        .unwrap();
    creds.remember("unused", "unrelated key".into());
    assert!(original.current(&store, Some(&creds), &providers).is_ok());

    // A replacement key read outside the host's registration path can have
    // the same counter value as the original. The request still must expire.
    let rotated = Credentials::new(true);
    rotated.remember("exa", "rotated".into());
    rotated.remember("unused", "unrelated key".into());
    assert_eq!(creds.generation(), rotated.generation());
    assert!(matches!(
        original.current(&store, Some(&rotated), &providers),
        Err(SearchConfigError::ConnectionChanged)
    ));

    creds.forget("exa");
    creds.remember("exa", "original".into());
    assert!(matches!(
        original.current(&store, Some(&creds), &providers),
        Err(SearchConfigError::ConnectionChanged)
    ));
    let resumed = resolve(&store, Some(&creds), &providers).unwrap().binding;
    let revision = repo.connection(exa.id.as_str()).unwrap().unwrap().revision;
    repo.delete_connection(&exa.id, revision, || Ok::<(), ene_store::StoreError>(()))
        .unwrap();
    repo.update_settings(repo.settings_revision().unwrap(), &[(exa, 0)])
        .unwrap();
    assert!(matches!(
        resumed.current(&store, Some(&creds), &providers),
        Err(SearchConfigError::ConnectionChanged)
    ));
}
