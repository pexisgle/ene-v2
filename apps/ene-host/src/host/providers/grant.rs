//! What a provider plugin may do over HTTP.
//!
//! The plugin declares the origins it talks to and how it authenticates; the
//! host decides whether that declaration is honoured. A plugin's request never
//! carries the credential itself, so a provider author cannot read, log or
//! forward it, and the host compares the destination against the origins the
//! operator installed rather than against anything the plugin sends at call
//! time.
use crate::host::{model::Credentials, settings::Connection};
use ene_plugin::{Failure, ProviderConfiguration, provider::AuthProfile};
use ene_plugin_runtime::service::HttpGrants;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use url::Url;

/// One provider's grant: its declared origins and its credential.
pub(crate) struct Grant {
    /// The origins the manifest declared, as `scheme://host[:port]`.
    origins: Vec<String>,
    auth: AuthProfile,
    /// The value the host attaches, resolved from the keyring or the
    /// provider's environment variable. Never handed to the plugin.
    secret: Option<String>,
}

impl Grant {
    /// The grant for `connection`, whose plugin declared `provider`.
    pub(crate) fn new(
        provider: &ProviderConfiguration,
        connection: &Connection,
        credentials: Option<&Credentials>,
    ) -> crate::host::error::Result<Self> {
        Ok(Self {
            origins: provider
                .destinations
                .iter()
                .filter_map(|destination| origin_of(destination))
                .collect(),
            auth: provider.auth.clone(),
            secret: crate::host::model::connection_secret(connection, credentials)?,
        })
    }
}

impl HttpGrants for Grant {
    fn allows(&self, _: &str, url: &Url) -> bool {
        // An exact origin match: a declared `https://api.example` does not
        // authorize `https://api.example.evil`, and a redirect cannot move
        // the request because the broker keeps redirects off.
        let origin = url.origin().ascii_serialization();
        self.origins.contains(&origin)
    }

    fn authorize(&self, _: &str, headers: &mut HeaderMap) -> Result<(), Failure> {
        let Some(secret) = &self.secret else {
            // No credential is configured. A provider that needs one fails at
            // its endpoint, which is the honest outcome.
            return Ok(());
        };
        let (name, value) = match &self.auth {
            AuthProfile::None => return Ok(()),
            AuthProfile::Bearer => (reqwest::header::AUTHORIZATION, format!("Bearer {secret}")),
            AuthProfile::Header { name } => (
                HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| Failure::new("invalid_auth_header"))?,
                secret.clone(),
            ),
            // A query credential cannot be attached as a header. The broker
            // refuses rather than sending the request unauthenticated.
            AuthProfile::Query { .. } => return Err(Failure::new("unsupported_auth")),
        };
        let value = HeaderValue::from_str(&value).map_err(|_| Failure::new("invalid_auth"))?;
        headers.insert(name, value);
        Ok(())
    }
}

/// The origin of `destination`, or `None` when it is not an absolute URL.
fn origin_of(destination: &str) -> Option<String> {
    Url::parse(destination)
        .ok()
        .map(|url| url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(origins: &[&str], auth: AuthProfile, secret: Option<&str>) -> Grant {
        Grant {
            origins: origins
                .iter()
                .filter_map(|origin| origin_of(origin))
                .collect(),
            auth,
            secret: secret.map(str::to_owned),
        }
    }

    #[test]
    fn only_a_declared_origin_is_allowed() {
        let grant = grant(&["https://api.example"], AuthProfile::Bearer, None);
        for allowed in ["https://api.example/v1/chat", "https://api.example"] {
            assert!(
                grant.allows("p", &Url::parse(allowed).unwrap()),
                "{allowed}"
            );
        }
        for denied in [
            "https://api.example.evil/v1",
            "http://api.example/v1",
            "https://other.example/v1",
        ] {
            assert!(!grant.allows("p", &Url::parse(denied).unwrap()), "{denied}");
        }
    }

    #[test]
    fn the_host_attaches_the_credential_and_the_plugin_never_sees_it() {
        let grant = grant(&[], AuthProfile::Bearer, Some("secret"));
        let mut headers = HeaderMap::new();
        grant.authorize("p", &mut headers).unwrap();
        assert_eq!(headers["authorization"], "Bearer secret");
    }

    #[test]
    fn a_named_header_carries_the_credential_verbatim() {
        let grant = grant(
            &[],
            AuthProfile::Header {
                name: "x-api-key".into(),
            },
            Some("secret"),
        );
        let mut headers = HeaderMap::new();
        grant.authorize("p", &mut headers).unwrap();
        assert_eq!(headers["x-api-key"], "secret");
    }

    #[test]
    fn a_query_credential_is_refused_rather_than_sent_unauthenticated() {
        let grant = grant(
            &[],
            AuthProfile::Query { name: "key".into() },
            Some("secret"),
        );
        let mut headers = HeaderMap::new();
        assert_eq!(
            grant.authorize("p", &mut headers).unwrap_err().code,
            "unsupported_auth"
        );
    }

    #[test]
    fn no_configured_credential_attaches_nothing() {
        let grant = grant(&[], AuthProfile::Bearer, None);
        let mut headers = HeaderMap::new();
        grant.authorize("p", &mut headers).unwrap();
        assert!(headers.is_empty());
    }
}
