//! Connection inspection, search adapter predicates, and wire input builders.
use ene_rpc as w;

/// Parameters for creating or updating a search connection input.
#[derive(Clone, Debug)]
pub struct SearchConnectionSpec {
    pub id: String,
    pub name: String,
    pub adapter: String,
    pub endpoint: String,
    pub enabled: bool,
    pub expected_revision: u64,
    pub max_results: u32,
}

pub fn llm_endpoint(conn: &w::Connection) -> Option<&w::ConnectionEndpoint> {
    match conn.config.as_option()?.config.as_ref()? {
        w::connection_config::Config::Llm(llm) => llm.endpoint.as_option(),
        _ => None,
    }
}

pub fn can_start_chat(conn: &w::Connection, providers: &[w::LlmProvider]) -> bool {
    conn.enabled
        && conn.kind == w::ConnectionKind::Llm
        && !needs_key(conn, providers)
        && conn
            .last_check
            .as_option()
            .is_none_or(|check| check.healthy)
}

pub fn search_adapter_of(conn: &w::Connection) -> Option<&str> {
    if conn.kind != w::ConnectionKind::Search {
        return None;
    }
    match conn.config.as_option()?.config.as_ref()? {
        w::connection_config::Config::Search(s) => s
            .endpoint
            .as_option()
            .map(|e| e.adapter.trim())
            .filter(|a| !a.is_empty()),
        _ => None,
    }
}

pub fn search_adapter_requires_key(adapter: &str) -> bool {
    matches!(
        adapter.trim().to_ascii_lowercase().as_str(),
        "exa" | "tavily" | "brave"
    )
}

pub fn search_connection_input(spec: SearchConnectionSpec) -> w::ConnectionInput {
    w::ConnectionInput {
        id: spec.id,
        name: spec.name,
        enabled: spec.enabled,
        expected_revision: spec.expected_revision,
        kind: w::ConnectionKind::Search.into(),
        config: Some(w::ConnectionConfig {
            config: Some(w::connection_config::Config::Search(Box::new(
                w::SearchConnection {
                    endpoint: Some(w::ConnectionEndpoint {
                        adapter: spec.adapter,
                        endpoint: spec.endpoint,
                        options_schema: String::new(),
                        options: "{}".into(),
                        ..Default::default()
                    })
                    .into(),
                    max_results: spec.max_results,
                    ..Default::default()
                },
            ))),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    }
}

pub fn needs_key(conn: &w::Connection, providers: &[w::LlmProvider]) -> bool {
    if matches!(conn.credential_state.as_str(), "registered" | "environment") {
        return false;
    }
    if search_adapter_of(conn).is_some_and(search_adapter_requires_key) {
        return true;
    }
    llm_endpoint(conn).is_some_and(|ep| {
        providers.iter().any(|p| {
            p.requires_credentials
                && p.adapter == ep.adapter
                && (ep.endpoint.trim().is_empty()
                    || p.endpoint.trim_end_matches('/') == ep.endpoint.trim().trim_end_matches('/'))
        })
    })
}

pub fn llm_provider_input(
    id: String,
    provider: w::LlmProvider,
    model_ids: Vec<String>,
) -> w::ConnectionInput {
    w::ConnectionInput {
        id,
        name: provider.name,
        enabled: true,
        expected_revision: 0,
        kind: w::ConnectionKind::Llm.into(),
        config: Some(w::ConnectionConfig {
            config: Some(w::connection_config::Config::Llm(Box::new(
                w::LlmConnection {
                    endpoint: Some(w::ConnectionEndpoint {
                        adapter: provider.adapter,
                        endpoint: provider.endpoint,
                        options_schema: String::new(),
                        options: "{}".into(),
                        ..Default::default()
                    })
                    .into(),
                    model_ids,
                    ..Default::default()
                },
            ))),
            ..Default::default()
        })
        .into(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_model_list_does_not_make_an_unregistered_connection_ready() {
        let provider = w::LlmProvider {
            requires_credentials: true,
            adapter: "openrouter".into(),
            endpoint: "https://openrouter.ai/api/v1".into(),
            ..Default::default()
        };
        let input = llm_provider_input("first".into(), provider.clone(), vec![]);
        let mut conn = w::Connection {
            enabled: true,
            kind: input.kind,
            config: input.config,
            last_check: Some(w::ConnectionCheck {
                healthy: true,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        let catalog = std::slice::from_ref(&provider);
        assert!(!can_start_chat(&conn, catalog));
        conn.credential_state = "registered".into();
        assert!(can_start_chat(&conn, catalog));
        conn.last_check = Some(w::ConnectionCheck::default()).into();
        assert!(!can_start_chat(&conn, catalog));
    }

    #[test]
    fn search_connections_detect_key_requirements_for_exa_and_tavily() {
        let make_search = |id: &str, adapter: &str, endpoint: &str, cred_state: &str| {
            let input = search_connection_input(SearchConnectionSpec {
                id: id.into(),
                name: format!("Web検索 ({adapter})"),
                adapter: adapter.into(),
                endpoint: endpoint.into(),
                enabled: true,
                expected_revision: 0,
                max_results: 5,
            });
            w::Connection {
                id: input.id,
                name: input.name,
                enabled: input.enabled,
                kind: input.kind,
                config: input.config,
                credential_state: cred_state.into(),
                revision: 1,
                ..Default::default()
            }
        };

        let exa_absent = make_search("search-exa", "exa", "https://api.exa.ai/search", "absent");
        let exa_registered = make_search(
            "search-exa",
            "exa",
            "https://api.exa.ai/search",
            "registered",
        );
        let tavily_absent = make_search(
            "search-tavily",
            "tavily",
            "https://api.tavily.com/search",
            "absent",
        );
        let tavily_env = make_search(
            "search-tavily",
            "tavily",
            "https://api.tavily.com/search",
            "environment",
        );
        let mwmbl_absent = make_search(
            "search-mwmbl",
            "mwmbl",
            "https://api.mwmbl.org/api/v2/search/",
            "absent",
        );

        assert_eq!(search_adapter_of(&exa_absent), Some("exa"));
        assert_eq!(search_adapter_of(&tavily_absent), Some("tavily"));
        assert!(needs_key(&exa_absent, &[]));
        assert!(!needs_key(&exa_registered, &[]));
        assert!(needs_key(&tavily_absent, &[]));
        assert!(!needs_key(&tavily_env, &[]));
        assert!(!needs_key(&mwmbl_absent, &[]));
    }
}
