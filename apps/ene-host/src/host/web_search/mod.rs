//! Public web search for the companion and its work agent, using saved
//! `Search` connections (`exa`, `tavily`, `brave`, `mwmbl`, `searxng`).
//!
//! This file keeps the tool itself: resolving a connection, probing it from
//! settings, and executing a bounded request. The library registers the tool
//! and schedules its result. The adapter module owns the provider wire formats.
mod adapter;
mod tool;

pub(crate) use tool::SearchPlugin;

use super::{
    db::Store,
    error::Failure,
    model::{Credentials, connection_secret},
    net,
    providers::Providers,
    settings::{Check, Connection, ConnectionConfig},
};
use adapter::{contains_credential, parse_adapter_results, read_http_error, send_search_request};
use bevy_ecs::prelude::Component;
use ene_catalog::SearchAdapter;
use ene_domain::{ConnectionId, SearchSelection};
use ene_policy::{CandidateAction, SearchApprovalSummary, SearchScope, WebSearchAction};
use ene_store::util;
use hmac::{Hmac, KeyInit, Mac};
use rig_core::tool::{ToolErrorKind, ToolExecutionError};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tool::{SearchArgs, SearchInputError};
use url::Url;

// `tests.rs` reaches `json!` through `use super::*`, as it did before the split.
#[cfg(test)]
use {super::settings::ConnectionKind, serde_json::json};

pub(crate) const NAME: &str = "web_search";
const MAX_QUERY_CHARS: usize = 500;
const MAX_RESULTS: usize = 20;
const DEFAULT_RESULTS: usize = 5;
const MAX_BODY_BYTES: usize = 512 * 1024;
const TIMEOUT: Duration = Duration::from_secs(25);

fn search_endpoint(adapter: SearchAdapter, configured: &str) -> Result<Url, SearchConfigError> {
    let provider = adapter.provider().name;
    let invalid = || SearchConfigError::InvalidEndpoint(provider);
    let mut url = Url::parse(configured).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid());
    }
    if ene_policy::hard_denied_search_destination(url.as_str(), url.host_str()) {
        return Err(SearchConfigError::BlockedDestination);
    }
    if adapter.provider().key_environment.is_some()
        && url.scheme() == "http"
        && !is_loopback_ip(&url)
    {
        return Err(SearchConfigError::InsecureCredentialRoute);
    }

    // Resolve provider-specific request paths before policy sees the destination.
    match adapter {
        SearchAdapter::Mwmbl if !url.path().ends_with('/') => {
            url.path_segments_mut().map_err(|()| invalid())?.push("");
        }
        SearchAdapter::Searxng if !url.path().trim_end_matches('/').ends_with("/search") => {
            url.path_segments_mut()
                .map_err(|()| invalid())?
                .pop_if_empty()
                .push("search");
        }
        _ => {}
    }
    let reserved: &[&str] = match adapter {
        SearchAdapter::Searxng => &["q", "format"],
        SearchAdapter::Brave => &["q", "count"],
        SearchAdapter::Mwmbl => &["q"],
        SearchAdapter::Exa | SearchAdapter::Tavily => &[],
    };
    if url
        .query_pairs()
        .any(|(name, _)| reserved.contains(&name.as_ref()))
    {
        return Err(SearchConfigError::ReservedParameters(provider));
    }
    url.set_fragment(None);
    Ok(url)
}

fn is_loopback_ip(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    }
}

/// The route the permission gate evaluated. A held call cannot switch to
/// a replacement connection or to a different revision of the same one.
#[derive(Clone)]
pub(crate) struct SearchBinding {
    connection: ConnectionId,
    created_revision: u64,
    revision: u64,
    credential_generation: u64,
    adapter: SearchAdapter,
    pub(crate) endpoint: Url,
    pub(crate) display_destination: String,
    secret: Option<Arc<str>>,
    max_results: Option<usize>,
}

impl SearchBinding {
    fn approval_scope(
        &self,
        key: &[u8; 32],
        label: String,
        query: &str,
        limit: usize,
    ) -> Option<SearchScope> {
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key).ok()?;
        mac.update(b"ene-host web_search v1\0");
        let provider = self.adapter.provider();
        for field in [
            self.connection.as_str().as_bytes(),
            &self.created_revision.to_be_bytes(),
            provider.adapter_id.as_bytes(),
            self.endpoint.as_str().as_bytes(),
            self.secret.as_deref().unwrap_or_default().as_bytes(),
            query.as_bytes(),
            &(limit as u64).to_be_bytes(),
        ] {
            mac.update(&(field.len() as u64).to_be_bytes());
            mac.update(field);
        }
        Some(SearchScope::new(mac.finalize().into_bytes().into(), label))
    }

    pub(crate) fn current(
        &self,
        store: &Store,
        credentials: Option<&Credentials>,
        providers: &Providers,
    ) -> Result<(), SearchConfigError> {
        let resolved = resolve(store, credentials, providers)?;
        let current = &resolved.binding;
        use subtle::ConstantTimeEq as _;
        let same_secret = match (self.secret.as_deref(), current.secret.as_deref()) {
            (None, None) => true,
            (Some(before), Some(after)) => before.as_bytes().ct_eq(after.as_bytes()).into(),
            _ => false,
        };
        if self.connection != current.connection
            || self.created_revision != current.created_revision
            || self.revision != current.revision
            || self.credential_generation != current.credential_generation
            || self.adapter != current.adapter
            || self.endpoint != current.endpoint
            || !same_secret
        {
            return Err(SearchConfigError::ConnectionChanged);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct ResolvedSearch {
    pub(crate) provider_name: String,
    pub(crate) binding: SearchBinding,
}

#[derive(Clone, Component)]
pub(crate) struct PreparedSearch {
    pub(crate) route: ResolvedSearch,
    pub(crate) action: Arc<WebSearchAction>,
}

impl PreparedSearch {
    pub(crate) fn candidate(&self) -> CandidateAction {
        CandidateAction::WebSearch(Arc::clone(&self.action))
    }

    pub(crate) fn summary(&self) -> SearchApprovalSummary {
        let binding = &self.route.binding;
        let id = binding.connection.as_str();
        SearchApprovalSummary {
            provider: binding.adapter.provider().name.to_owned(),
            connection_name: self.route.provider_name.clone(),
            connection_id: if contains_credential(id, binding.secret.as_deref()) {
                "非表示".to_owned()
            } else {
                id.to_owned()
            },
            query: self.action.query.clone(),
            limit: self.action.limit as u32,
        }
    }
}

impl ResolvedSearch {
    pub(crate) fn prepare(
        self,
        arguments: &Value,
        credentials: Option<&Credentials>,
    ) -> Result<PreparedSearch, SearchInputError> {
        let args = SearchArgs::try_from(arguments)?;
        let binding = &self.binding;
        let (query, limit) = args.effective(binding.max_results)?;
        if contains_credential(query, binding.secret.as_deref()) {
            return Err(SearchInputError::CredentialInQuery);
        }
        let scope = credentials
            .and_then(|credentials| credentials.approval_key.as_ref())
            .and_then(|key| {
                let label = format!(
                    "{} · {} · {query:?} · {limit} · {}",
                    binding.adapter.provider().name,
                    self.provider_name,
                    binding.display_destination,
                );
                let label = if contains_credential(&label, binding.secret.as_deref()) {
                    binding.adapter.provider().name.into()
                } else {
                    util::truncate(&label, 600)
                };
                binding.approval_scope(key, label, query, limit)
            });
        let action = Arc::new(WebSearchAction {
            endpoint: binding.endpoint.clone(),
            query: query.to_owned(),
            limit,
            raw_arguments: arguments.clone(),
            scope,
        });
        Ok(PreparedSearch {
            route: self,
            action,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum SearchConfigError {
    #[error("could not read search settings: {0}")]
    Settings(#[from] ene_store::StoreError),
    #[error(
        "search_unconfigured: Web検索を使うには設定画面で有効な Search 接続（Exa / Tavily / Brave / Mwmbl / SearXNG）を登録してください"
    )]
    Unconfigured,
    #[error(
        "search_unconfigured: 有効な Web 検索接続が複数あります。設定画面で1件を選択してください"
    )]
    AmbiguousSelection,
    #[error("search_unconfigured: invalid Search connection configuration")]
    WrongKind,
    #[error("search_unconfigured: unsupported search adapter `{0}`")]
    UnsupportedAdapter(String),
    #[error("search_unconfigured: `{0}` connection requires an endpoint URL")]
    MissingEndpoint(&'static str),
    #[error("search_unconfigured: invalid {0} HTTP(S) endpoint")]
    InvalidEndpoint(&'static str),
    #[error("the web search destination is hard-blocked")]
    BlockedDestination,
    #[error("HTTPS is required for search providers with credentials outside a loopback IP")]
    InsecureCredentialRoute,
    #[error("search_unconfigured: {0} endpoint supplies parameters owned by the search tool")]
    ReservedParameters(&'static str),
    #[error("search_unconfigured: `{0}` search connection requires an API key credential")]
    MissingCredential(&'static str),
    #[error(transparent)]
    Credential(#[from] super::error::HostError),
    #[error("the search connection changed before execution")]
    ConnectionChanged,
}

/// Resolves the selected connection before an outbound query can be authorized.
pub(crate) fn resolve(
    store: &Store,
    credentials: Option<&Credentials>,
    providers: &Providers,
) -> Result<ResolvedSearch, SearchConfigError> {
    let enabled = store.settings_repo().enabled_search_connections()?;
    match SearchSelection::from_enabled(enabled.iter()) {
        SearchSelection::Off => Err(SearchConfigError::Unconfigured),
        SearchSelection::Ambiguous => Err(SearchConfigError::AmbiguousSelection),
        SearchSelection::Selected((connection, created_revision)) => {
            resolve_connection(connection, credentials, providers, *created_revision)
        }
    }
}

pub(crate) fn resolve_connection(
    connection: &Connection,
    credentials: Option<&Credentials>,
    providers: &Providers,
    created_revision: u64,
) -> Result<ResolvedSearch, SearchConfigError> {
    let ConnectionConfig::Search(cfg) = &connection.config else {
        return Err(SearchConfigError::WrongKind);
    };
    let adapter = SearchAdapter::parse(&cfg.endpoint.adapter)
        .ok_or_else(|| SearchConfigError::UnsupportedAdapter(cfg.endpoint.adapter.clone()))?;
    let provider = adapter.provider();
    let configured = cfg.endpoint.endpoint.trim();
    let endpoint = if configured.is_empty() {
        provider
            .default_endpoint
            .ok_or(SearchConfigError::MissingEndpoint(provider.name))?
    } else {
        configured
    };
    let endpoint = search_endpoint(adapter, endpoint)?;
    let keyed = provider.key_environment.is_some();
    let credential_generation = if keyed {
        credentials.map_or(0, |credentials| {
            credentials.generation_of(connection.id.as_str())
        })
    } else {
        0
    };
    let secret = if keyed {
        let secret = connection_secret(connection, credentials, providers)?
            .filter(|value| !value.trim().is_empty())
            .ok_or(SearchConfigError::MissingCredential(provider.name))?;
        if credentials.is_some_and(|credentials| {
            credentials.generation_of(connection.id.as_str()) != credential_generation
        }) {
            return Err(SearchConfigError::ConnectionChanged);
        }
        Some(Arc::from(secret.trim()))
    } else {
        None
    };
    let display_path = &endpoint[..url::Position::AfterPath];
    let display_destination = if contains_credential(display_path, secret.as_deref()) {
        provider.name.to_owned()
    } else {
        display_path.to_owned()
    };
    let provider_name = if connection.name.trim().is_empty()
        || contains_credential(&connection.name, secret.as_deref())
    {
        provider.name.to_owned()
    } else {
        connection.name.clone()
    };
    let max_results =
        (cfg.max_results > 0).then_some((cfg.max_results as usize).clamp(1, MAX_RESULTS));
    Ok(ResolvedSearch {
        provider_name,
        binding: SearchBinding {
            connection: connection.id.clone(),
            created_revision,
            revision: connection.revision,
            credential_generation,
            adapter,
            endpoint,
            display_destination,
            secret,
            max_results,
        },
    })
}

/// Probes a `Search` connection from Settings -> CheckConnection.
pub(crate) async fn probe(
    connection: &Connection,
    credentials: &Credentials,
    providers: &Providers,
) -> Check {
    let checked_at = util::now();
    let failure = match resolve_connection(connection, Some(credentials), providers, 0) {
        Ok(resolved) => execute(&resolved, "rust programming", 1, || Ok(()))
            .await
            .err()
            .map(|error| Failure::new("connection_check", error.to_string())),
        Err(error) => Some(Failure::new("connection_check", error.to_string())),
    };
    Check {
        healthy: failure.is_none(),
        checked_at,
        failure,
        ..Default::default()
    }
}

async fn execute(
    resolved: &ResolvedSearch,
    query: &str,
    limit: usize,
    authorize: impl FnOnce() -> Result<(), ToolExecutionError> + Send,
) -> Result<Value, ToolExecutionError> {
    tokio::time::timeout(TIMEOUT, async {
        let endpoint = &resolved.binding.endpoint;
        let client = if endpoint.scheme() == "http" && is_loopback_ip(endpoint) {
            net::loopback_client()
        } else {
            net::client()
        }
        .map_err(|error| ToolExecutionError::other(error.to_string()))?;
        let response = send_search_request(&client, resolved, query, limit, authorize).await?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ToolExecutionError::other(format!(
                "{} quota or rate limit reached",
                resolved.binding.adapter.provider().name
            )));
        }
        if !status.is_success() {
            return Err(read_http_error(response, resolved, status).await);
        }
        let body = net::read_response_body(
            response,
            MAX_BODY_BYTES,
            resolved.binding.adapter.provider().name,
        )
        .await
        .map_err(|error| ToolExecutionError::other(error.to_string()))?;
        parse_adapter_results(
            resolved.binding.adapter,
            &resolved.provider_name,
            &body,
            limit,
            resolved.binding.secret.as_deref(),
        )
    })
    .await
    .map_err(|_| ToolExecutionError::new(ToolErrorKind::Timeout, "Web search timed out"))?
}

#[cfg(test)]
mod scope_tests;
#[cfg(test)]
mod tests;
