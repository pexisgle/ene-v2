use ene_core::{
    domain::{Connection, Error, ErrorKind},
    ports::{Result, llm::*},
};
use ene_plugin::{
    ProviderConfiguration,
    provider::{AuthProfile, BrokerRouting},
};
use serde_json::{Value, json};
#[derive(Clone)]
pub struct Bundle {
    pub name: String,
    pub manifest: ProviderConfiguration,
    pub revision: String,
}
fn invalid() -> Error {
    Error::invalid()
}
pub fn valid_endpoint(u: &url::Url) -> Result<()> {
    if !(u.scheme() == "https"
        || u.scheme() == "http"
            && [Some("127.0.0.1"), Some("[::1]"), Some("localhost")].contains(&u.host_str()))
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok(())
}
fn validate_options(m: &ProviderConfiguration, options: &Value) -> Result<()> {
    let object = options.as_object().ok_or_else(invalid)?;
    if object
        .keys()
        .any(|k| !m.settings.iter().any(|f| f.key == *k))
    {
        return Err(invalid());
    }
    for f in &m.settings {
        match object.get(&f.key) {
            None if !f.required => {}
            None => return Err(invalid()),
            Some(v) => {
                let valid = match f.kind.as_str() {
                    "text" => v.as_str().is_some_and(|s| s.len() <= 8192),
                    "number" => v.is_number(),
                    "boolean" => v.is_boolean(),
                    "select" => v.as_str().is_some_and(|s| f.choices.iter().any(|c| c == s)),
                    _ => false,
                };
                if !valid {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(())
}
impl Bundle {
    pub fn descriptor(
        &self,
        state: &str,
        capabilities: Capabilities,
        error: Option<String>,
    ) -> Descriptor {
        Descriptor {
            id: self.manifest.id.clone(),
            name: self.name.clone(),
            kind: "plugin".into(),
            endpoint: self.manifest.endpoint.clone(),
            options: ene_rpc::JsonDocument {
                json: self.manifest.options.to_string(),
                ..Default::default()
            }
            .into(),
            settings: self.manifest.settings.clone(),
            requires_credentials: !matches!(self.manifest.auth, AuthProfile::None),
            environment_variable: self.manifest.environment_variable.clone(),
            destinations: self.manifest.destinations.clone(),
            state: state.into(),
            error,
            revision: self.revision.clone(),
            capabilities: capabilities.into(),
            models: self.manifest.models.clone(),
            ..Default::default()
        }
    }
    pub fn policy(&self, c: &Connection) -> Result<Policy> {
        if c.kind != "llm" {
            return Err(invalid());
        }
        let endpoint = url::Url::parse(&c.endpoint).map_err(|_| invalid())?;
        valid_endpoint(&endpoint)?;
        if !self
            .manifest
            .destinations
            .contains(&endpoint.origin().ascii_serialization())
        {
            return Err(Error::new(ErrorKind::PermissionRequired));
        }
        validate_options(
            &self.manifest,
            &serde_json::from_str::<Value>(&c.options.json).map_err(|_| invalid())?,
        )?;
        Ok(self.identity_policy(c))
    }
    /// The verified package's scope can change even if it rejects an old endpoint.
    pub fn identity_policy(&self, c: &Connection) -> Policy {
        Policy {
            revision: self.revision.clone(),
            fingerprint: json!({"adapter":c.adapter,"endpoint":c.endpoint,"options":serde_json::from_str::<Value>(&c.options.json).unwrap_or(Value::Null),"implementation":self.revision,"destinations":self.manifest.destinations,"auth":self.manifest.auth}),
            destinations: self.manifest.destinations.clone(),
            requires_credentials: !matches!(self.manifest.auth, AuthProfile::None),
            environment_variable: self.manifest.environment_variable.clone(),
        }
    }
}

pub fn validate(manifest: &ProviderConfiguration) -> Result<()> {
    if manifest.id == super::super::openai::ID
        || !ene_plugin_runtime::package::identifier(&manifest.id)
    {
        return Err(invalid());
    }
    let endpoint = url::Url::parse(&manifest.endpoint).map_err(|_| invalid())?;
    valid_endpoint(&endpoint)?;
    if manifest.destinations.is_empty()
        || !manifest
            .destinations
            .iter()
            .any(|d| d == endpoint.origin().ascii_serialization().as_str())
    {
        return Err(invalid());
    }
    for d in &manifest.destinations {
        if d.starts_with("http") {
            let u = url::Url::parse(d).map_err(|_| invalid())?;
            valid_endpoint(&u)?;
            if u.origin().ascii_serialization() != *d {
                return Err(invalid());
            }
        } else if d.is_empty() || d.len() > 100 || d.chars().any(char::is_control) {
            return Err(invalid());
        }
    }
    match &manifest.routing {
        BrokerRouting::Direct => {
            if manifest
                .destinations
                .iter()
                .any(|d| !d.starts_with("https://") && !d.starts_with("http://"))
                || endpoint.host_str() == Some("openrouter.ai")
            {
                return Err(invalid());
            }
        }
        BrokerRouting::OpenRouter { only } => {
            if endpoint.as_str().trim_end_matches('/') != "https://openrouter.ai/api/v1"
                || only.is_empty()
                || !manifest
                    .destinations
                    .contains(&"https://openrouter.ai".to_owned())
                || only
                    .iter()
                    .any(|d| d.starts_with("http") || !manifest.destinations.contains(d))
                || manifest.destinations.iter().any(|d| {
                    d != "https://openrouter.ai" && !only.contains(d) && !d.starts_with("https://")
                })
            {
                return Err(invalid());
            }
        }
    }
    if let Some(name) = &manifest.environment_variable
        && (name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'))
    {
        return Err(invalid());
    }
    match &manifest.auth {
        AuthProfile::Header { name } => {
            let h = ene_outbound::http::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| invalid())?;
            if [
                "host",
                "content-length",
                "connection",
                "cookie",
                "proxy-authorization",
            ]
            .contains(&h.as_str())
            {
                return Err(invalid());
            }
        }
        AuthProfile::Query { name }
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
        {
            return Err(invalid());
        }
        _ => {}
    }
    for (name, value) in &manifest.headers {
        let header = ene_outbound::http::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| invalid())?;
        ene_outbound::http::header::HeaderValue::from_str(value).map_err(|_| invalid())?;
        if [
            "authorization",
            "proxy-authorization",
            "cookie",
            "host",
            "content-length",
            "connection",
            "transfer-encoding",
            "expect",
            "upgrade",
            "te",
            "trailer",
        ]
        .contains(&header.as_str())
            || matches!(&manifest.auth,AuthProfile::Header{name} if name.eq_ignore_ascii_case(header.as_str()))
        {
            return Err(invalid());
        }
    }
    let mut keys = std::collections::HashSet::new();
    for field in &manifest.settings {
        if !matches!(
            field.kind.as_str(),
            "text" | "number" | "boolean" | "select"
        ) || field.key.is_empty()
            || !keys.insert(&field.key)
            || field.label.is_empty()
            || field.kind == "select" && field.choices.is_empty()
        {
            return Err(invalid());
        }
    }
    validate_options(manifest, &manifest.options)?;
    Ok(())
}
