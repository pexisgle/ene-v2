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
