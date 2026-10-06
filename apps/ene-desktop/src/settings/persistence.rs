//! Terminal settings and transactional drafts; serialization remains compatible.
use atomic_write_file::{AtomicWriteFile, unix::OpenOptionsExt as _};
use ene_protocol::avatar::AvatarSettings;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientSettings {
    pub font_family: String,
    pub text_size: i32,
    pub theme_index: i32,
    pub avatar: AvatarSettings,
}
impl Default for ClientSettings {
    fn default() -> Self {
        let mut avatar = AvatarSettings::default();
        let cache = crate::infrastructure::platform::cache();
        avatar.model = cache
            .join("models/Seed-san.vrm")
            .to_string_lossy()
            .into_owned();
        avatar.motion = Some(
            cache
                .join("motions/three-vrm-sample.vrma")
                .to_string_lossy()
                .into_owned(),
        );
        Self {
            font_family: "Noto Sans CJK JP".into(),
            text_size: 16,
            theme_index: 0,
            avatar,
        }
    }
}
pub fn config_path() -> PathBuf {
    crate::infrastructure::platform::config().join("client.json")
}
impl ClientSettings {
    pub fn load(path: &Path) -> Result<Self, String> {
        match fs::read(path) {
            Ok(bytes) => {
                let mut settings: Self =
                    serde_json::from_slice(&bytes).map_err(|e| format!("設定を読めません: {e}"))?;
                settings.avatar.validate()?;
                settings.theme_index = settings.theme_index.clamp(0, 2);
                settings.text_size = settings.text_size.clamp(12, 28);
                Ok(settings)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("設定を読めません: {e}")),
        }
    }
    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.avatar.validate()?;
        let directory = path.parent().ok_or("設定フォルダがありません")?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(|e| format!("設定フォルダを作れません: {e}"))?;
        let result = (|| -> std::io::Result<()> {
            let mut file = AtomicWriteFile::options()
                .mode(0o600)
                .preserve_mode(false)
                .preserve_owner(false)
                .open(path)?;
            serde_json::to_writer_pretty(&mut file, self)?;
            file.write_all(b"\n")?;
            file.commit()
        })();
        result.map_err(|e| format!("設定を保存できません: {e}"))
    }
}
