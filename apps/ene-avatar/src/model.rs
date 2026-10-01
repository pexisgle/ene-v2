use binrw::BinRead;
use gltf_json::validation::Validate;
use std::{
    fs::File,
    io::{Read, Seek},
    path::Path,
};

#[binrw::binread]
#[br(little, magic = b"glTF")]
struct Header {
    #[br(temp, assert(version == 2))]
    version: u32,
    #[br(assert(length >= 20))]
    length: u32,
}
#[binrw::binread]
#[br(little, import(expected: [u8; 4], remaining: u64))]
struct Chunk {
    #[br(assert(length.is_multiple_of(4) && u64::from(length) + 8 <= remaining))]
    length: u32,
    #[br(temp, assert(kind == expected))]
    kind: [u8; 4],
}
pub fn validate(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let value = read_glb_json(path)?;
    let extensions: bevy_vrm1::prelude::VrmExtensions =
        serde_json::from_value(value["extensions"].clone())?;
    let vrm = extensions.vrmc_vrm;
    if vrm.spec_version != "1.0" {
        return Err("VRM 1.0に対応しています。".into());
    }
    if vrm
        .humanoid
        .human_bones
        .values()
        .any(|bone| bone.node >= value["nodes"].as_array().map_or(0, Vec::len))
    {
        return Err("VRMの骨指定が不正です。".into());
    }
    let meta = vrm.meta.ok_or("VRMの利用条件がありません。")?;
    Ok(format!(
        "{} / {} / {}",
        meta.name.as_deref().unwrap_or("VRM"),
        meta.authors.join(", "),
        meta.license_url
            .as_deref()
            .unwrap_or("モデルの利用条件を確認してください")
    ))
}
pub fn read_glb_json(path: &Path) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    read_container(&mut file, file_len)
}
fn read_container(
    file: &mut (impl Read + Seek),
    file_len: u64,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let header = Header::read(file)?;
    if u64::from(header.length) != file_len {
        return Err("GLBのファイル長が不正です。".into());
    }
    let chunk = Chunk::read_args(file, (*b"JSON", file_len - 12))?;
    if chunk.length > 16 * 1024 * 1024 {
        return Err("モデルのJSON情報が大きすぎます。".into());
    }
    let mut json = vec![0; chunk.length as usize];
    file.read_exact(&mut json)?;
    let remaining = file_len - 20 - u64::from(chunk.length);
    if remaining != 0 {
        let binary = Chunk::read_args(file, (*b"BIN\0", remaining))?;
        if u64::from(binary.length) + 8 != remaining {
            return Err("GLBのバイナリチャンクが不正です。".into());
        }
        // BIN is checked against the opened file's length and never read here.
    }
    let value: serde_json::Value = serde_json::from_slice(&json)?;
    let mut root: gltf_json::Root = serde_json::from_slice(&json)?;
    // VRM extensions are validated below/by bevy_vrm1, not registered in gltf-json.
    root.extensions_required.retain(|name| {
        !matches!(
            name.as_str(),
            "VRMC_vrm"
                | "VRMC_vrm_animation"
                | "VRMC_springBone"
                | "VRMC_node_constraint"
                | "VRMC_materials_mtoon"
                | "VRMC_materials_hdr_emissiveMultiplier"
        )
    });
    let mut errors = Vec::new();
    root.validate(&root, gltf_json::Path::new, &mut |path, error| {
        errors.push(format!("{}: {error:?}", path().as_str()))
    });
    if !errors.is_empty() {
        return Err(format!("glTF情報が不正です: {}", errors.join(", ")).into());
    }
    for uri in root
        .buffers
        .iter()
        .filter_map(|b| b.uri.as_deref())
        .chain(root.images.iter().filter_map(|i| i.uri.as_deref()))
    {
        if !uri.starts_with("data:") {
            return Err("外部ファイルを参照するモデルは利用できません。".into());
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_vrm_one_with_embedded_resources_is_accepted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("日本語.vrm");
        for (json, valid) in [
            (
                serde_json::json!({"asset":{"version":"2.0"},"extensions":{"VRMC_vrm":{"specVersion":"1.0","humanoid":{"humanBones":{}},"meta":{"allowAntisocialOrHateUsage":false,"allowExcessivelySexualUsage":false,"allowExcessivelyViolentUsage":false,"allowPoliticalOrReligiousUsage":false,"allowRedistribution":false,"name":"日本語","authors":["作者"],"licenseUrl":"https://example.com"}}}}),
                true,
            ),
            (
                serde_json::json!({"asset":{"version":"2.0"},"extensions":{"VRM":{}}}),
                false,
            ),
            (
                serde_json::json!({"asset":{"version":"2.0"},"extensions":{"VRMC_vrm":{"specVersion":"1.0"}},"images":[{"uri":"../secret.png"}]}),
                false,
            ),
        ] {
            let mut bytes = serde_json::to_vec(&json).unwrap();
            while !bytes.len().is_multiple_of(4) {
                bytes.push(b' ');
            }
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

#[cfg(test)]
mod container_tests {
    use super::*;
    use std::io::{Cursor, Write};
    fn container(json: serde_json::Value, binary: u32) -> Vec<u8> {
        let mut json = serde_json::to_vec(&json).unwrap();
        while !json.len().is_multiple_of(4) {
            json.push(b' ');
        }
        let length = 20 + json.len() as u32 + if binary == 0 { 0 } else { 8 + binary };
        let mut bytes = Vec::new();
        bytes.extend(b"glTF");
        bytes.extend(2u32.to_le_bytes());
        bytes.extend(length.to_le_bytes());
        bytes.extend((json.len() as u32).to_le_bytes());
        bytes.extend(b"JSON");
        bytes.extend(json);
        if binary != 0 {
            bytes.extend(binary.to_le_bytes());
            bytes.extend(b"BIN\0");
        }
        bytes
    }
    #[test]
    fn malformed_containers_references_and_external_uris_are_rejected() {
        let valid = container(serde_json::json!({"asset":{"version":"2.0"}}), 0);
        assert!(read_container(&mut Cursor::new(&valid), valid.len() as u64).is_ok());
        for position in [0, 4, 8, 12, 16] {
            let mut bytes = valid.clone();
            bytes[position] ^= 1;
            assert!(read_container(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
        }
        assert!(
            read_container(
                &mut Cursor::new(&valid[..valid.len() - 1]),
                valid.len() as u64
            )
            .is_err()
        );
        for json in [
            serde_json::json!({"asset":{"version":"2.0"},"nodes":[{"mesh":3}]}),
            serde_json::json!({"asset":{"version":"2.0"},"buffers":[{"byteLength":4,"uri":"../private.bin"}]}),
        ] {
            let bytes = container(json, 0);
            assert!(read_container(&mut Cursor::new(&bytes), bytes.len() as u64).is_err());
        }
        let mut bytes = container(serde_json::json!({"asset":{"version":"2.0"}}), 4);
        let length = bytes.len();
        bytes[length - 4] = b'X';
        assert!(read_container(&mut Cursor::new(bytes), length as u64 + 4).is_err());
    }
    #[test]
    fn sparse_large_binary_is_accepted_without_reading_its_payload() {
        let binary = 300 * 1024 * 1024;
        let bytes = container(
            serde_json::json!({"asset":{"version":"2.0"},"buffers":[{"byteLength":binary}]}),
            binary,
        );
        let length = bytes.len() as u64 + u64::from(binary);
        // Cursor has only headers and JSON: reading any BIN byte would fail.
        assert!(read_container(&mut Cursor::new(&bytes), length).is_ok());
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&bytes).unwrap();
        file.as_file().set_len(length).unwrap();
        assert!(read_glb_json(file.path()).is_ok());
    }
}
