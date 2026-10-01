use std::{fs::File, io::Read, path::Path};

pub fn validate(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let value = read_glb_json(path)?;
    let vrm = &value["extensions"]["VRMC_vrm"];
    if vrm["specVersion"] != "1.0" {
        return Err("VRM 1.0に対応しています。VRM 0.xは利用できません。".into());
    }
    let meta = &vrm["meta"];
    let authors = meta["authors"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    Ok(format!(
        "{} / {} / {}",
        meta["name"].as_str().unwrap_or("VRM"),
        authors,
        meta["licenseUrl"]
            .as_str()
            .unwrap_or("モデルの利用条件を確認してください")
    ))
}
pub fn read_glb_json(path: &Path) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let mut file = File::open(path).map_err(|error| {
        format!("VRMモデルを読めません（{error}）。設定でモデルを選び直してください。")
    })?;
    let mut header = [0; 20];
    file.read_exact(&mut header)?;
    if &header[..4] != b"glTF"
        || u32::from_le_bytes(header[4..8].try_into()?) != 2
        || &header[16..20] != b"JSON"
    {
        return Err("VRM 1.0のGLBファイルを選択してください。".into());
    }
    let len = u32::from_le_bytes(header[12..16].try_into()?) as usize;
    if len > 16 * 1024 * 1024 {
        return Err("モデルのJSON情報が大きすぎます。".into());
    }
    let mut json = vec![0; len];
    file.read_exact(&mut json)?;
    let value: serde_json::Value = serde_json::from_slice(&json)?;
    // Embedded images/buffers only: a selected avatar cannot load adjacent files or URLs.
    for kind in ["buffers", "images"] {
        if let Some(entries) = value[kind].as_array() {
            for entry in entries {
                if let Some(uri) = entry["uri"].as_str()
                    && !uri.starts_with("data:")
                {
                    return Err("外部ファイルを参照するモデルは利用できません。埋め込み形式にしてください。".into());
                }
            }
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_vrm_one_with_embedded_resources_is_accepted() {
        let path = std::env::temp_dir().join(format!("ene-model-{}.vrm", std::process::id()));
        for (json, valid) in [
            (
                serde_json::json!({"extensions":{"VRMC_vrm":{"specVersion":"1.0","meta":{"name":"日本語","authors":["作者"],"licenseUrl":"https://example.com"}}}}),
                true,
            ),
            (serde_json::json!({"extensions":{"VRM":{}}}), false),
            (
                serde_json::json!({"extensions":{"VRMC_vrm":{"specVersion":"1.0"}},"images":[{"uri":"../secret.png"}]}),
                false,
            ),
        ] {
            let bytes = serde_json::to_vec(&json).unwrap();
            let mut glb = b"glTF".to_vec();
            glb.extend(2u32.to_le_bytes());
            glb.extend((20u32 + bytes.len() as u32).to_le_bytes());
            glb.extend((bytes.len() as u32).to_le_bytes());
            glb.extend(b"JSON");
            glb.extend(bytes);
            std::fs::write(&path, glb).unwrap();
            assert_eq!(validate(&path).is_ok(), valid);
        }
        std::fs::remove_file(path).unwrap();
    }
}
