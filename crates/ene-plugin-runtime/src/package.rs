//! Packages are read without executing code. Executable files are captured and installed
//! in memory; plugins receive no filesystem access to code or assets.
use crate::{
    Error, Result,
    limits::{MAX_ASSET_BYTES, MAX_CODE},
};
use ene_plugin::*;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
#[derive(Clone, Debug)]
pub struct Package {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub revision: String,
    files: Arc<BTreeMap<String, Vec<u8>>>,
    stamps: Arc<BTreeMap<String, Stamp>>,
}
#[derive(Clone, Debug)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}
#[cfg(unix)]
fn stamp(metadata: &std::fs::Metadata) -> Stamp {
    Stamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        dev: metadata.dev(),
        ino: metadata.ino(),
        ctime: metadata.ctime(),
        ctime_nsec: metadata.ctime_nsec(),
    }
}
#[cfg(not(unix))]
fn stamp(metadata: &std::fs::Metadata) -> Stamp {
    Stamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    }
}
#[cfg(unix)]
fn unchanged(metadata: &std::fs::Metadata, stamp: &Stamp) -> bool {
    metadata.is_file()
        && metadata.len() == stamp.len
        && metadata.modified().ok() == stamp.modified
        && metadata.dev() == stamp.dev
        && metadata.ino() == stamp.ino
        && metadata.ctime() == stamp.ctime
        && metadata.ctime_nsec() == stamp.ctime_nsec
}
#[cfg(not(unix))]
fn unchanged(metadata: &std::fs::Metadata, stamp: &Stamp) -> bool {
    metadata.is_file() && metadata.len() == stamp.len && metadata.modified().ok() == stamp.modified
}
/// Portable package paths use forward slashes, never native device paths.
fn relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part.contains(':')
                && !part.contains(['<', '>', '"', '|', '?', '*'])
                && !part.ends_with(['.', ' '])
                && !matches!(
                    part.split('.')
                        .next()
                        .unwrap_or_default()
                        .trim_end_matches(' ')
                        .to_ascii_uppercase()
                        .as_str(),
                    "CON"
                        | "PRN"
                        | "AUX"
                        | "NUL"
                        | "COM1"
                        | "COM2"
                        | "COM3"
                        | "COM4"
                        | "COM5"
                        | "COM6"
                        | "COM7"
                        | "COM8"
                        | "COM9"
                        | "LPT1"
                        | "LPT2"
                        | "LPT3"
                        | "LPT4"
                        | "LPT5"
                        | "LPT6"
                        | "LPT7"
                        | "LPT8"
                        | "LPT9"
                        | "CONIN$"
                        | "CONOUT$"
                        | "COM¹"
                        | "COM²"
                        | "COM³"
                        | "LPT¹"
                        | "LPT²"
                        | "LPT³"
                )
        })
}
pub fn identifier(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn owned(path: &Path, directory: bool) -> Result<()> {
    ene_platform::storage::check(path, directory, false).map_err(|_| Error::Invalid)
}
fn portable_name(names: &mut BTreeMap<String, (String, bool)>, path: &str) -> Result<()> {
    if !relative(path) {
        return Err(Error::Invalid);
    }
    let mut prefix = String::new();
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        let directory = parts.peek().is_some();
        match names.entry(prefix.to_lowercase()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((prefix.clone(), directory));
            }
            std::collections::btree_map::Entry::Occupied(entry)
                if directory && entry.get() == &(prefix.clone(), true) => {}
            _ => return Err(Error::Invalid),
        }
    }
    Ok(())
}
fn read(root: &Path, path: &str, limit: u64) -> Result<(Vec<u8>, Stamp)> {
    if !relative(path) {
        return Err(Error::Invalid);
    }
    let file = open_code(root, path)?;
    let m = file.metadata()?;
    if !m.is_file() || m.len() > limit {
        return Err(Error::Invalid);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Invalid);
    }
    Ok((bytes, stamp(&m)))
}
fn open_code(root: &Path, path: &str) -> Result<std::fs::File> {
    let mut parent = root.to_path_buf();
    let components: Vec<_> = Path::new(path).components().collect();
    for part in &components[..components.len() - 1] {
        parent.push(part);
        owned(&parent, true)?;
    }
    Ok(ene_platform::storage::open(&root.join(path), false, false)?)
}
fn hash_file(mut file: std::fs::File, limit: u64) -> Result<(String, Stamp)> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(Error::Invalid);
    }
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > limit {
            return Err(Error::Invalid);
        }
        digest.update(&buffer[..read]);
    }
    Ok((format!("{:x}", digest.finalize()), stamp(&metadata)))
}
impl Package {
    pub fn load(root: &Path) -> Result<Self> {
        owned(root, true)?;
        let (raw, manifest_stamp) = read(root, "manifest.json", 65536)?;
        let manifest: Manifest = serde_json::from_slice(&raw)?;
        if manifest.contract_version != CONTRACT_VERSION
            || !identifier(&manifest.id)
            || manifest.id == "openai"
            || manifest.name.trim().is_empty()
            || manifest.version.is_empty()
            || manifest.entrypoints.is_empty()
            || manifest.entrypoints.len() > 32
            || manifest.assets.len() > 256
        {
            return Err(Error::Invalid);
        }
        let mut ids = BTreeSet::new();
        let mut providers = BTreeSet::new();
        for e in &manifest.entrypoints {
            if !identifier(&e.id)
                || !ids.insert(&e.id)
                || !relative(&e.file)
                || e.provides.is_empty()
                || e.provides.len() > 6
                || e.uses.len() > 8
                || (!e.window_events.is_empty() || e.uses.contains(&Service::WindowEvents))
                    && (e.target != Target::Desktop
                        || !e.provides.contains(&Extension::Character)
                        || !e.uses.contains(&Service::WindowEvents))
                || e.provides.iter().collect::<BTreeSet<_>>().len() != e.provides.len()
                || e.uses.iter().collect::<BTreeSet<_>>().len() != e.uses.len()
            {
                return Err(Error::Invalid);
            }
            if let Some(p) = &e.provider {
                if !e.provides.contains(&Extension::Llm)
                    || !identifier(&p.id)
                    || p.id == "openai"
                    || !providers.insert(&p.id)
                {
                    return Err(Error::Invalid);
                }
            } else if e.provides.contains(&Extension::Llm) {
                return Err(Error::Invalid);
            }
        }
        let mut files = BTreeMap::from([("manifest.json".to_string(), raw)]);
        let mut stamps = BTreeMap::from([("manifest.json".to_string(), manifest_stamp)]);
        let mut portable_names = BTreeMap::new();
        portable_name(&mut portable_names, "manifest.json")?;
        let mut total = files["manifest.json"].len() as u64;
        for path in manifest
            .entrypoints
            .iter()
            .map(|e| &e.file)
            .collect::<BTreeSet<_>>()
        {
            if files.contains_key(path) {
                return Err(Error::Invalid);
            }
            portable_name(&mut portable_names, path)?;
            let (data, stamp) = read(root, path, MAX_CODE)?;
            total += data.len() as u64;
            if total > MAX_CODE {
                return Err(Error::Invalid);
            }
            files.insert(path.clone(), data);
            stamps.insert(path.clone(), stamp);
        }
        for asset in manifest.assets.values() {
            portable_name(&mut portable_names, &asset.path)?;
            if files.contains_key(&asset.path)
                || asset.size > MAX_ASSET_BYTES
                || asset.sha256.len() != 64
                || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(Error::Invalid);
            }
            let (digest, stamp) = hash_file(open_code(root, &asset.path)?, MAX_ASSET_BYTES)?;
            if stamp.len != asset.size || digest != asset.sha256.to_ascii_lowercase() {
                return Err(Error::Invalid);
            }
            stamps.insert(asset.path.clone(), stamp);
        }
        let mut hash = Sha256::new();
        for (path, bytes) in &files {
            hash.update((path.len() as u64).to_le_bytes());
            hash.update(path.as_bytes());
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        for (name, asset) in &manifest.assets {
            hash.update((name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            hash.update((asset.path.len() as u64).to_le_bytes());
            hash.update(asset.path.as_bytes());
            hash.update(asset.size.to_le_bytes());
            hash.update(asset.sha256.to_ascii_lowercase().as_bytes());
        }
        Ok(Self {
            root: root.to_path_buf(),
            manifest,
            revision: format!("{:x}", hash.finalize()),
            files: Arc::new(files),
            stamps: Arc::new(stamps),
        })
    }
    pub fn verify(&self) -> Result<()> {
        let same = self.stamps.iter().all(|(path, stamp)| {
            std::fs::symlink_metadata(self.root.join(path))
                .is_ok_and(|metadata| unchanged(&metadata, stamp))
        });
        // ctime/inode make the fast path content-sensitive on Unix. Windows has
        // no equivalent std metadata, so always re-read and hash there.
        #[cfg(unix)]
        if same {
            return Ok(());
        }
        #[cfg(not(unix))]
        let _ = same;
        if Self::load(&self.root)?.revision != self.revision {
            return Err(Error::Changed);
        }
        Ok(())
    }
    pub fn entry(&self, id: &str) -> Result<&EntryPoint> {
        self.manifest
            .entrypoints
            .iter()
            .find(|e| e.id == id)
            .ok_or(Error::Unsupported)
    }
    pub fn code(&self, path: &str) -> Result<&[u8]> {
        self.files
            .get(path)
            .map(Vec::as_slice)
            .ok_or(Error::Invalid)
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn package_paths_cannot_name_devices_or_streams() {
        for path in [
            "../x", "/x", "C:/x", "x:stream", "x\\y", "a/./b", "a//b", "NUL.txt", "com1", "a.",
            "a ",
        ] {
            assert!(!super::relative(path), "{path}");
        }
        assert!(super::relative("日本語/main.wasm"));
    }
    use super::*;
    #[test]
    fn package_components_reject_case_aliases_and_file_directory_collisions() {
        let mut names = BTreeMap::new();
        portable_name(&mut names, "code/a.wasm").unwrap();
        portable_name(&mut names, "code/b.wasm").unwrap();
        for path in ["Code/c.wasm", "code/A.wasm", "code/a.wasm/child", "code"] {
            assert!(portable_name(&mut names.clone(), path).is_err(), "{path}");
        }
    }
    fn fixture(dir: &Path) {
        ene_platform::storage::create_private_dir_all(dir).unwrap();
        ene_platform::storage::write_atomic(&dir.join("main.wasm"), b"return {}").unwrap();
        ene_platform::storage::write_atomic(&dir.join("manifest.json"), serde_json::json!({"contract_version":ene_plugin::CONTRACT_VERSION,"id":"sample","name":"Sample","version":"1","entrypoints":[{"id":"behavior","target":"host","file":"main.wasm","provides":["character"]}]}).to_string().as_bytes()).unwrap();
    }
    #[test]
    fn window_subscriptions_require_a_desktop_character_entrypoint_and_service() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("package");
        fixture(&root);
        let path = root.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["entrypoints"][0]["window_events"] = serde_json::json!([{
            "applications": ["editor"], "events": ["focused"]
        }]);
        let write = |manifest: &serde_json::Value| {
            ene_platform::storage::write_atomic(&path, manifest.to_string().as_bytes()).unwrap();
        };
        write(&manifest);
        assert!(Package::load(&root).is_err());
        manifest["entrypoints"][0]["uses"] = serde_json::json!(["window_events"]);
        write(&manifest);
        assert!(Package::load(&root).is_err());
        manifest["entrypoints"][0]["target"] = serde_json::json!("desktop");
        write(&manifest);
        Package::load(&root).unwrap();
        manifest["entrypoints"][0]["provides"] = serde_json::json!(["harness"]);
        write(&manifest);
        assert!(Package::load(&root).is_err());
    }

    #[test]
    fn snapshots_capture_code_and_source_changes_invalidate_revision() {
        let source = tempfile::tempdir().unwrap();
        let source_root = source.path().join("package");
        fixture(&source_root);
        let package = Package::load(&source_root).unwrap();
        std::fs::write(source_root.join("main.wasm"), "return {changed=true}").unwrap();
        assert!(matches!(package.verify(), Err(Error::Changed)));
        assert_eq!(package.code("main.wasm").unwrap(), b"return {}");
    }
    #[test]
    fn asset_contents_are_hashed_not_only_sized() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("package");
        fixture(&root);
        std::fs::write(root.join("voice.bin"), b"hello").unwrap();
        let path = root.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        manifest["assets"] = serde_json::json!({
            "voice": {
                "path": "voice.bin",
                "sha256": format!("{:x}", Sha256::digest(b"hello")),
                "size": 5
            }
        });
        ene_platform::storage::write_atomic(&path, manifest.to_string().as_bytes()).unwrap();
        Package::load(&root).unwrap();
        std::fs::write(root.join("voice.bin"), b"world").unwrap();
        assert!(matches!(Package::load(&root), Err(Error::Invalid)));
    }
    #[test]
    fn same_size_and_restored_mtime_still_invalidates_verification() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("package");
        fixture(&root);
        let package = Package::load(&root).unwrap();
        let file = root.join("main.wasm");
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, b"return {x").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert!(matches!(package.verify(), Err(Error::Changed)));
    }
    #[test]
    fn entrypoints_share_a_single_code_snapshot_and_budget() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("package");
        fixture(&root);
        let path = root.join("manifest.json");
        let mut manifest: Manifest =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut entry = manifest.entrypoints[0].clone();
        entry.id = "second".into();
        manifest.entrypoints.push(entry);
        ene_platform::storage::write_atomic(&path, &serde_json::to_vec(&manifest).unwrap())
            .unwrap();
        let code = std::fs::OpenOptions::new()
            .write(true)
            .open(root.join("main.wasm"))
            .unwrap();
        code.set_len(MAX_CODE / 2 + 1).unwrap();
        let package = Package::load(&root).unwrap();
        assert_eq!(package.files.len(), 2);
        assert_eq!(
            package.code("main.wasm").unwrap().len() as u64,
            MAX_CODE / 2 + 1
        );
        code.set_len(MAX_CODE).unwrap();
        assert!(matches!(Package::load(&root), Err(Error::Invalid)));
    }
    #[test]
    fn legacy_manifests_are_rejected() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("package");
        fixture(&root);
        let path = root.join("manifest.json");
        let current: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for field in ["contract_version", "runtime", "code_files"] {
            let mut manifest = current.clone();
            match field {
                "contract_version" => manifest[field] = 2.into(),
                "runtime" => manifest["entrypoints"][0][field] = "extism".into(),
                _ => manifest[field] = serde_json::json!(["main.wasm"]),
            }
            ene_platform::storage::write_atomic(&path, &serde_json::to_vec(&manifest).unwrap())
                .unwrap();
            assert!(Package::load(&root).is_err(), "{field}");
        }
    }
    #[test]
    fn module_traversal_and_links_are_rejected() {
        assert!(!relative("../secret"));
        assert!(!relative("/secret"));
        assert!(!relative("a/./b"));
        let source = tempfile::tempdir().unwrap();
        let source_root = source.path().join("package");
        fixture(&source_root);
        std::fs::remove_file(source_root.join("main.wasm")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", source_root.join("main.wasm")).unwrap();
        #[cfg(windows)]
        std::fs::hard_link(
            source_root.join("manifest.json"),
            source_root.join("main.wasm"),
        )
        .unwrap();
        assert!(Package::load(&source_root).is_err());
    }
}
