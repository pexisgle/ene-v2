use crate::{
    actor::{Actor, id},
    configuration::validate_profile,
    error::{invalid, missing},
    store,
};
use anyhow::Result;
use ene_rpc as w;
use serde_json::Value;
use std::path::Path;

impl Actor {
    pub(crate) async fn export_character(
        &self,
        request: w::ExportRequest,
    ) -> Result<w::CharacterPackage> {
        let mut db = self.db.acquire().await?;
        let character = store::get::<w::Character>(&mut db, "character", &request.id)
            .await?
            .ok_or_else(|| missing("character not found"))?;
        let mut package = w::CharacterPackage {
            version: 1,
            profile: if request.profile {
                character.profile
            } else {
                Default::default()
            },
            voice: request.voice.then_some(character.voice),
            ..Default::default()
        };
        if request.avatar && !character.avatar.is_empty() {
            let path = Path::new(&character.avatar);
            match read_avatar(path).await {
                Ok(bytes) if can_redistribute(&bytes) => {
                    package.avatar = bytes;
                    package.avatar_name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .map(str::to_owned);
                }
                _ => package
                    .warnings
                    .push("再配布許可を確認できないため、モデルを除外しました。".into()),
            }
        }
        Ok(package)
    }

    pub(crate) async fn import_character(
        &mut self,
        request: w::ImportRequest,
    ) -> Result<w::Character> {
        let key = store::key(request.mutation.as_option())?;
        let hash = store::fingerprint(&("import_character", &request))?;
        let mut db = self.db.acquire().await?;
        if let Some(saved) = store::replay(&mut db, &key.request_key, &hash).await? {
            return Ok(saved);
        }
        let package = request
            .package
            .into_option()
            .ok_or_else(|| invalid("character package required"))?;
        if package.version != 1 || package.avatar.len() > 32 * 1024 * 1024 {
            return Err(invalid("unsupported character package"));
        }
        let mut character = if request.character_id.is_empty() {
            w::Character {
                id: id(),
                profile: Some(w::Profile {
                    name: "コンパニオン".into(),
                    ..Default::default()
                })
                .into(),
                ..Default::default()
            }
        } else {
            store::get(&mut db, "character", &request.character_id)
                .await?
                .ok_or_else(|| missing("character not found"))?
        };
        if package.profile.is_set() {
            validate_profile(&package.profile)?;
            character.profile = package.profile;
        }
        if let Some(voice) = package.voice {
            character.voice = voice;
        }
        if !package.avatar.is_empty() {
            if model_metadata(&package.avatar).is_none() {
                return Err(invalid("invalid VRM asset"));
            }
            let path = self
                .state
                .join("avatars")
                .join(format!("{}.vrm", store::fingerprint(&package.avatar)?));
            if !tokio::fs::try_exists(&path).await? {
                let staging = self.state.join("avatars").join(format!("{}.tmp", id()));
                tokio::fs::write(&staging, &package.avatar).await?;
                tokio::fs::rename(&staging, &path).await?;
            }
            character.avatar = path.to_string_lossy().into_owned();
        }
        character.revision = key.revision + 1;
        drop(db);
        let id = character.id.clone();
        let saved =
            crate::configuration::save(&self.db, "character", key, &id, &hash, character).await?;
        self.announce_snapshot().await?;
        Ok(saved)
    }
}

async fn read_avatar(path: &Path) -> Result<Vec<u8>> {
    if tokio::fs::metadata(path).await?.len() > 32 * 1024 * 1024 {
        return Err(invalid("avatar asset exceeds size limit"));
    }
    Ok(tokio::fs::read(path).await?)
}

fn model_metadata(data: &[u8]) -> Option<Value> {
    if data.get(..4)? != b"glTF" || data.get(16..20)? != b"JSON" {
        return None;
    }
    let length = u32::from_le_bytes(data.get(12..16)?.try_into().ok()?) as usize;
    let document: Value = serde_json::from_slice(data.get(20..20 + length)?).ok()?;
    let extensions = document.get("extensions")?;
    extensions
        .get("VRMC_vrm")
        .or_else(|| extensions.get("VRM"))?
        .get("meta")
        .cloned()
}

fn can_redistribute(data: &[u8]) -> bool {
    model_metadata(data).is_some_and(|meta| {
        meta["allowRedistribution"].as_bool().unwrap_or_else(|| {
            matches!(
                meta["licenseName"].as_str(),
                Some(
                    "CC0"
                        | "CC_BY"
                        | "CC_BY_NC"
                        | "CC_BY_SA"
                        | "CC_BY_NC_SA"
                        | "CC_BY_ND"
                        | "CC_BY_NC_ND"
                )
            )
        })
    })
}
