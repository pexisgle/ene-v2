use anyhow::{Result, ensure};
use ene_core::avatar::Command;
use ene_plugin::{
    CharacterAction, CharacterEvent, CharacterTarget, Event, Extension, Operation, Target,
    plugin_runtime::{
        Package,
        instance::{DenyServices, Instance},
    },
};
use ene_rpc::{Capability, DeviceOperation};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) struct ClientPlugin {
    package: Package,
    entry: String,
    name: String,
    body: bool,
    perception: bool,
}

impl ClientPlugin {
    pub(crate) fn load(paths: &[PathBuf]) -> Result<Vec<Self>> {
        let mut plugins = Vec::<Self>::new();
        for path in paths {
            let package = Package::load(path)?;
            let before = plugins.len();
            for entry in package
                .manifest
                .entrypoints
                .iter()
                .filter(|entry| entry.target == Target::Client)
            {
                let name = format!("body.plugin.{}.{}", package.manifest.id, entry.id);
                let perception = entry.provides.contains(&Extension::Perception);
                ensure!(
                    !plugins
                        .iter()
                        .any(|other| other.name == name || perception && other.perception),
                    "duplicate desktop extension or sensor"
                );
                plugins.push(Self {
                    package: package.clone(),
                    entry: entry.id.clone(),
                    name,
                    body: entry.provides.contains(&Extension::Character),
                    perception,
                });
            }
            ensure!(plugins.len() > before, "package has no desktop entrypoints");
        }
        ensure!(plugins.len() <= 16, "too many desktop extensions");
        Ok(plugins)
    }

    pub(crate) fn capability(&self) -> Option<Capability> {
        self.body.then(|| Capability {
            name: self.name.clone(),
            schema_json: json!({
                "$comment": self.package.revision,
                "type":"object",
                "properties":{"name":{"type":"string","minLength":1,"maxLength":128}},
                "required":["name"],
                "additionalProperties":false
            })
            .to_string(),
            ..Default::default()
        })
    }

    pub(crate) fn serves_body(&self, capability: &str) -> bool {
        self.body && self.name == capability
    }

    pub(crate) fn serves_perception(&self) -> bool {
        self.perception
    }

    async fn invoke(
        &self,
        operation: Operation,
        cancel: &CancellationToken,
    ) -> Result<(Value, Option<Event>)> {
        let mut instance = Instance::load_for_target(
            self.package.clone(),
            self.entry.clone(),
            Target::Client,
            cancel,
        )
        .await?;
        let (send, mut receive) = mpsc::channel(2);
        let (value, event) = tokio::try_join!(
            instance.invoke(operation, Arc::new(DenyServices), send, cancel),
            async {
                let first = receive.recv().await;
                ensure!(
                    receive.recv().await.is_none(),
                    "one desktop effect was expected"
                );
                Ok(first)
            }
        )?;
        Ok((value, event))
    }

    pub(crate) async fn observe(
        &self,
        application: &str,
        title: &str,
        idle: Option<bool>,
        cancel: &CancellationToken,
    ) -> Result<(String, String)> {
        let input = |text: &str| text[..text.floor_char_boundary(text.len().min(1024))].to_owned();
        let (value, _) = self
            .invoke(
                Operation::Observe {
                    application: input(application),
                    title: input(title),
                    idle,
                },
                cancel,
            )
            .await?;
        let text = |key| -> Result<String> {
            let field = value.get(key).and_then(Value::as_str).unwrap_or_default();
            ensure!(
                field.len() <= 1000,
                "plugin observation exceeds the size limit"
            );
            Ok(field.to_owned())
        };
        Ok((text("observation")?, text("interpretation")?))
    }

    pub(crate) async fn body(
        &self,
        operation: &DeviceOperation,
        device: &str,
        data: Value,
        assets: &Path,
        cancel: &CancellationToken,
    ) -> Result<Command> {
        let character = operation.companion_id.clone();
        let (_, effect) = self
            .invoke(
                Operation::Character {
                    target: CharacterTarget {
                        device_id: device.into(),
                        character_id: character.clone(),
                    },
                    event: CharacterEvent::Custom {
                        name: operation.capability.clone(),
                        data,
                    },
                },
                cancel,
            )
            .await?;
        match effect {
            Some(Event::Character {
                action: CharacterAction::Expression { name, .. },
                ..
            }) => Ok(Command::Expression {
                character,
                name: (name != "neutral").then_some(name),
            }),
            Some(Event::Character {
                action: CharacterAction::Motion { asset, .. },
                ..
            }) => {
                ensure!(
                    !asset.is_empty()
                        && asset
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                    "invalid motion preset"
                );
                let directory = assets.join("vrma").canonicalize()?;
                let path = directory.join(format!("{asset}.vrma")).canonicalize()?;
                ensure!(
                    path.starts_with(directory),
                    "motion outside installed presets"
                );
                Ok(Command::Motion {
                    character,
                    motion: Some(path.to_string_lossy().into_owned()),
                    looping: false,
                })
            }
            _ => anyhow::bail!("the plugin returned no permitted visual action"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires the compiled reference plugin at ENE_TEST_DESKTOP_PLUGIN"]
    async fn sandboxed_desktop_plugin_interprets_an_observation_and_renders_an_effect() -> Result<()>
    {
        let path = std::env::var_os("ENE_TEST_DESKTOP_PLUGIN").expect("reference plugin path");
        let plugins = ClientPlugin::load(&[PathBuf::from(path)])?;
        let cancel = CancellationToken::new();
        let plugin = &plugins[0];
        let (fact, interpretation) = plugin
            .observe("test-editor", "private buffer", None, &cancel)
            .await?;
        assert_eq!(fact, "test-editor");
        assert_eq!(interpretation, "前面アプリを検出");
        let capability = plugin.capability().expect("visual capability");
        assert_eq!(capability.name, "body.plugin.reference.desktop");
        assert!(capability.schema_json.contains(&plugin.package.revision));
        let command = plugin
            .body(
                &DeviceOperation {
                    capability: capability.name,
                    companion_id: "ene".into(),
                    ..Default::default()
                },
                "active-desktop",
                json!({"name":"happy"}),
                Path::new("."),
                &cancel,
            )
            .await?;
        assert!(
            matches!(command, Command::Expression { character, name: Some(name) }
            if character == "ene" && name == "happy")
        );
        Ok(())
    }
}
