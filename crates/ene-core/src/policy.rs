use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use url::Url;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Read,
    Write,
    Execute,
    Send,
    Delete,
    Payment,
    System,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Standard,
    FullAccess,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    Allow,
    AlwaysAllow,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Target {
    pub receiver: String,
    pub revision: String,
    pub operation: String,
    pub effect: Effect,
    pub device: Option<(String, u64)>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Action {
    target: Target,
    arguments: Map<String, Value>,
}

impl Action {
    pub fn new(target: Target, arguments: Map<String, Value>) -> Result<Self> {
        ensure!(
            !target.receiver.is_empty()
                && !target.revision.is_empty()
                && !target.operation.is_empty(),
            "incomplete operation identity"
        );
        let action = Self { target, arguments };
        ensure!(
            serde_json_canonicalizer::to_vec(&action)?.len() <= 64 * 1024,
            "operation arguments exceed limit"
        );
        Ok(action)
    }

    pub const fn target(&self) -> &Target {
        &self.target
    }

    pub const fn arguments(&self) -> &Map<String, Value> {
        &self.arguments
    }

    pub fn digest(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(
            serde_json_canonicalizer::to_vec(self)?,
        )))
    }

    pub fn rule_digest(&self) -> Result<String> {
        let mut scope = self.clone();
        if let Some((_, generation)) = &mut scope.target.device {
            *generation = 0;
        }
        scope.digest()
    }

    pub fn verify(&self, expected: &str) -> Result<()> {
        ensure!(
            self.digest()? == expected,
            "operation changed after approval"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Rule {
    pub digest: String,
    pub allow: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Gate {
    Deny,
    Card,
    Assess,
    Allow,
}

pub fn gate(
    action: &Action,
    rules: &[Rule],
    mode: Mode,
    require_confirmation: bool,
) -> Result<Gate> {
    let digest = action.rule_digest()?;
    let matching = || rules.iter().filter(|rule| rule.digest == digest);
    Ok(
        if hard_denied(action) || matching().any(|rule| !rule.allow) {
            Gate::Deny
        } else if mode == Mode::FullAccess {
            Gate::Allow
        } else if require_confirmation
            || matches!(action.target.effect, Effect::Delete | Effect::Execute)
        {
            Gate::Card
        } else if matching().any(|rule| rule.allow) {
            Gate::Allow
        } else {
            Gate::Assess
        },
    )
}

pub fn hard_denied(action: &Action) -> bool {
    let effect = action.target.effect;
    if matches!(effect, Effect::Payment | Effect::System) {
        return true;
    }
    let name = action.target.operation.to_ascii_lowercase();
    if [
        "payment",
        "banking",
        "securities",
        "crypto_transfer",
        "wire_transfer",
    ]
    .iter()
    .any(|part| name.contains(part))
    {
        return true;
    }
    action
        .arguments
        .iter()
        .any(|(key, value)| prohibited_argument(key, value, effect))
}

fn prohibited_argument(key: &str, value: &Value, effect: Effect) -> bool {
    match value {
        Value::Object(entries) => entries
            .iter()
            .any(|(name, value)| prohibited_argument(name, value, effect)),
        Value::Array(values) => values
            .iter()
            .any(|value| prohibited_argument(key, value, effect)),
        Value::String(value) => {
            let key = key.to_ascii_lowercase();
            if matches!(key.as_str(), "url" | "uri" | "endpoint")
                && Url::parse(value).is_ok_and(|url| !safe_url(&url))
            {
                return true;
            }
            if effect != Effect::Read
                && matches!(
                    key.as_str(),
                    "path" | "file" | "file_path" | "directory" | "dir"
                )
            {
                let path = value.replace('\\', "/").to_ascii_lowercase();
                return path.split('/').any(|part| part == "..")
                    || ["/etc", "/boot", "/dev", "/system", "c:/windows"]
                        .iter()
                        .any(|base| {
                            path == *base
                                || path
                                    .strip_prefix(base)
                                    .is_some_and(|rest| rest.starts_with('/'))
                        });
            }
            // Shell arguments cannot be reliably classified by a substring denylist.
            matches!(key.as_str(), "command" | "cmd" | "script")
        }
        _ => false,
    }
}

pub fn safe_url(url: &Url) -> bool {
    matches!(url.scheme(), "https" | "http")
        && url.username().is_empty()
        && url.password().is_none()
        && match url.host() {
            Some(url::Host::Domain(name)) => {
                let name = name.trim_end_matches('.');
                !matches!(name, "metadata.google.internal" | "metadata.azure.com")
                    && !name.ends_with(".metadata.google.internal")
            }
            Some(url::Host::Ipv4(ip)) => {
                !ip.is_link_local() && !ip.is_unspecified() && !ip.is_multicast()
            }
            Some(url::Host::Ipv6(ip)) => {
                !ip.is_unicast_link_local()
                    && !ip.is_unspecified()
                    && !ip.is_multicast()
                    && ip.to_ipv4_mapped().is_none_or(|ip| {
                        !ip.is_link_local() && !ip.is_unspecified() && !ip.is_multicast()
                    })
            }
            None => false,
        }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assessment {
    pub allow: bool,
    pub confidence: f64,
}

impl Assessment {
    pub fn permits(&self) -> bool {
        self.allow && (0.9..=1.0).contains(&self.confidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn action(effect: Effect) -> Result<Action> {
        Action::new(
            Target {
                receiver: "mail".into(),
                revision: "revision".into(),
                operation: "send_message".into(),
                effect,
                device: None,
            },
            serde_json::from_value(
                json!({"recipient": "someone@example.org", "subject": "Report"}),
            )?,
        )
    }

    #[test]
    fn binding_covers_destination_and_arguments() -> Result<()> {
        let original = action(Effect::Send)?;
        let digest = original.digest()?;
        let mut changed = original.clone();
        changed.target.device = Some(("device".into(), 2));
        assert!(changed.verify(&digest).is_err());
        changed.target.device = None;
        changed.arguments.insert("subject".into(), json!("Changed"));
        assert!(changed.verify(&digest).is_err());
        original.verify(&digest)
    }

    #[test]
    fn rule_cannot_override_deny_or_required_card() -> Result<()> {
        let action = action(Effect::Send)?;
        let mut rules = vec![Rule {
            digest: action.digest()?,
            allow: true,
        }];
        assert_eq!(gate(&action, &rules, Mode::Standard, true)?, Gate::Card);
        assert_eq!(gate(&action, &rules, Mode::Standard, false)?, Gate::Allow);
        rules.push(Rule {
            digest: action.digest()?,
            allow: false,
        });
        assert_eq!(gate(&action, &rules, Mode::FullAccess, false)?, Gate::Deny);
        let mut payment = action;
        payment.target.effect = Effect::Payment;
        assert_eq!(gate(&payment, &[], Mode::FullAccess, false)?, Gate::Deny);
        Ok(())
    }
}
