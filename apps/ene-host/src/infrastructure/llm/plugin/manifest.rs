use ene_core::{
    domain::{Connection, Error, ErrorKind},
    ports::{Result, llm::*},
};
use ene_protocol::provider::{AuthProfile, BrokerRouting, Manifest, SettingKind, VERSION};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};
#[derive(Clone)]
pub struct Bundle {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub revision: String,
}
fn invalid() -> Error {
    Error::invalid()
}
fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(path)
        .map_err(|_| invalid())?;
    let m = f.metadata().map_err(|_| invalid())?;
    if !m.is_file()
        || m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o022 != 0
        || m.nlink() != 1
        || m.len() > limit
    {
        return Err(invalid());
    }
    let mut data = Vec::new();
    f.take(limit + 1)
        .read_to_end(&mut data)
        .map_err(|_| invalid())?;
    if data.len() as u64 > limit {
        return Err(invalid());
    }
    Ok(data)
}
fn digest(root: &Path, hash: &mut Sha256, budget: &mut (u64, usize)) -> Result<()> {
    let mut paths = std::fs::read_dir(root)
        .map_err(|_| invalid())?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|_| invalid())?;
    paths.sort();
    for p in paths {
        budget.1 += 1;
        if budget.1 > 256 {
            return Err(invalid());
        }
        let m = std::fs::symlink_metadata(&p).map_err(|_| invalid())?;
        if m.file_type().is_symlink()
            || m.uid() != rustix::process::geteuid().as_raw()
            || m.mode() & 0o022 != 0
        {
            return Err(invalid());
        }
        hash.update(p.file_name().ok_or_else(invalid)?.as_encoded_bytes());
        if m.is_dir() {
            digest(&p, hash, budget)?;
        } else {
            let bytes = read(&p, 32 * 1024 * 1024)?;
            budget.0 += bytes.len() as u64;
            if budget.0 > 32 * 1024 * 1024 {
                return Err(invalid());
            }
            hash.update(bytes);
        }
    }
    Ok(())
}
pub fn load(root: &Path) -> Result<Bundle> {
    let m = std::fs::symlink_metadata(root).map_err(|_| invalid())?;
    if !m.is_dir()
        || m.file_type().is_symlink()
        || m.uid() != rustix::process::geteuid().as_raw()
        || m.mode() & 0o022 != 0
    {
        return Err(invalid());
    }
    let manifest: Manifest = serde_json::from_slice(&read(&root.join("manifest.json"), 65536)?)
        .map_err(|_| invalid())?;
    if manifest.protocol_version != VERSION
        || manifest.id == super::super::openai::ID
        || manifest.id.is_empty()
        || manifest.id.len() > 80
        || !manifest
            .id
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        || manifest.name.trim().is_empty()
        || manifest.command.is_empty()
        || manifest.command.len() > 32
    {
        return Err(invalid());
    }
    if manifest
        .command
        .iter()
        .any(|s| s.len() > 4096 || s.chars().any(char::is_control))
        || !(manifest.command[0].starts_with("/usr/bin/")
            || manifest.command[0].starts_with("/plugin/"))
    {
        return Err(invalid());
    }
    let endpoint = url::Url::parse(&manifest.endpoint).map_err(|_| invalid())?;
    valid_endpoint(&endpoint)?;
    if let Some(ene_protocol::provider::ModelCatalog::ModelsDev { provider }) =
        &manifest.model_catalog
        && (provider.is_empty()
            || provider.len() > 80
            || !provider
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || !manifest
                .destinations
                .iter()
                .any(|d| d == super::super::catalog::ORIGIN))
    {
        return Err(invalid());
    }
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
                || manifest.destinations
                    != std::iter::once("https://openrouter.ai".to_owned())
                        .chain(only.iter().cloned())
                        .chain(
                            manifest
                                .model_catalog
                                .as_ref()
                                .map(|_| super::super::catalog::ORIGIN.to_owned()),
                        )
                        .collect::<Vec<_>>()
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
            let h =
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
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
        let header =
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
        reqwest::header::HeaderValue::from_str(value).map_err(|_| invalid())?;
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
        if field.key.is_empty()
            || !keys.insert(&field.key)
            || field.label.is_empty()
            || matches!(field.kind, SettingKind::Select) && field.choices.is_empty()
        {
            return Err(invalid());
        }
    }
    validate_options(&manifest, &manifest.options)?;
    let mut hash = Sha256::new();
    digest(root, &mut hash, &mut (0, 0))?;
    let revision = format!("{:x}", hash.finalize());
    Ok(Bundle {
        root: root.to_path_buf(),
        manifest,
        revision,
    })
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
fn validate_options(m: &Manifest, options: &Value) -> Result<()> {
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
                let valid = match f.kind {
                    SettingKind::Text => v.as_str().is_some_and(|s| s.len() <= 8192),
                    SettingKind::Number => v.is_number(),
                    SettingKind::Boolean => v.is_boolean(),
                    SettingKind::Select => {
                        v.as_str().is_some_and(|s| f.choices.iter().any(|c| c == s))
                    }
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
            name: self.manifest.name.clone(),
            kind: "plugin".into(),
            endpoint: self.manifest.endpoint.clone(),
            options: self.manifest.options.clone(),
            settings: self.manifest.settings.clone(),
            requires_credentials: !matches!(self.manifest.auth, AuthProfile::None),
            environment_variable: self.manifest.environment_variable.clone(),
            destinations: self.manifest.destinations.clone(),
            state: state.into(),
            error,
            revision: self.revision.clone(),
            capabilities,
            models: self.manifest.models.clone(),
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
        validate_options(&self.manifest, &c.options)?;
        Ok(Policy {
            fingerprint: json!({"adapter":c.adapter,"endpoint":c.endpoint,"options":c.options,"implementation":self.revision,"destinations":self.manifest.destinations,"auth":self.manifest.auth}),
            destinations: self.manifest.destinations.clone(),
            requires_credentials: !matches!(self.manifest.auth, AuthProfile::None),
            environment_variable: self.manifest.environment_variable.clone(),
        })
    }
}
