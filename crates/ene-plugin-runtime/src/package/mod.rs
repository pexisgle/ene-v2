//! Packages are read without executing code. Executable files are captured and installed
//! in memory; plugins receive no filesystem access to the code.
mod paths;
mod read;

pub use paths::identifier;
use paths::{portable_name, relative};
use read::{Stamp, open_root, read, unchanged};

use crate::{Error, Result, limits::MAX_CODE};
use ene_plugin::*;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug)]
pub struct Package {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub revision: String,
    files: Arc<BTreeMap<String, Vec<u8>>>,
    stamps: Arc<BTreeMap<String, Stamp>>,
}
impl Package {
    pub fn load(root: &Path) -> Result<Self> {
        let directory = open_root(root)?;
        let (raw, manifest_stamp) = read(&directory, "manifest.json", 65536)?;
        let manifest: Manifest = serde_json::from_slice(&raw)?;
        if manifest.contract_version != CONTRACT_VERSION
            || !identifier(&manifest.id)
            || manifest.id == "openai"
            || manifest.name.trim().is_empty()
            || manifest.version.is_empty()
            || manifest.entrypoints.is_empty()
            || manifest.entrypoints.len() > 32
        {
            return Err(Error::Invalid);
        }
        let mut ids = BTreeSet::new();
        let mut providers = BTreeSet::new();
        for e in &manifest.entrypoints {
            let target_valid = match e.target {
                Target::Acp => {
                    e.file.is_empty()
                        && !e.command.is_empty()
                        && e.command.len() <= 64
                        && e.command.iter().all(|part| {
                            !part.is_empty()
                                && part.len() <= 4096
                                && !part.chars().any(char::is_control)
                        })
                        && e.provides == [Extension::Harness]
                        && e.uses.is_empty()
                        && e.provider.is_none()
                }
                Target::Host => relative(&e.file) && e.command.is_empty(),
            };
            if !identifier(&e.id)
                || !ids.insert(&e.id)
                || !target_valid
                || e.provides.is_empty()
                || e.provides.len() > 6
                || e.uses.len() > 8
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
            .filter(|file| !file.is_empty())
            .collect::<BTreeSet<_>>()
        {
            if files.contains_key(path) {
                return Err(Error::Invalid);
            }
            portable_name(&mut portable_names, path)?;
            let (data, stamp) = read(&directory, path, MAX_CODE)?;
            total += data.len() as u64;
            if total > MAX_CODE {
                return Err(Error::Invalid);
            }
            files.insert(path.clone(), data);
            stamps.insert(path.clone(), stamp);
        }
        let mut hash = Sha256::new();
        for (path, bytes) in &files {
            hash.update((path.len() as u64).to_le_bytes());
            hash.update(path.as_bytes());
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        Ok(Self {
            root: root.to_path_buf(),
            manifest,
            revision: hash
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            files: Arc::new(files),
            stamps: Arc::new(stamps),
        })
    }
    /// Metadata-only check for listings; enforcement uses `verify`.
    pub fn current(&self) -> bool {
        self.stamps.iter().all(|(path, stamp)| {
            std::fs::symlink_metadata(self.root.join(path))
                .is_ok_and(|metadata| unchanged(&metadata, stamp))
        })
    }
    pub fn verify(&self) -> Result<()> {
        // ctime/inode make the fast path content-sensitive on Unix. Windows has
        // no equivalent std metadata, so always re-read and hash there.
        #[cfg(unix)]
        {
            if self.current() {
                return Ok(());
            }
        }
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
    use super::paths::{portable_name, relative};
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
    fn directory(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    fn write_file(path: &Path, bytes: impl AsRef<[u8]>) {
        std::fs::write(path, bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
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
        directory(dir);
        write_file(&dir.join("main.wasm"), b"return {}");
        write_file(&dir.join("manifest.json"), serde_json::json!({"contract_version":ene_plugin::CONTRACT_VERSION,"id":"sample","name":"Sample","version":"1","entrypoints":[{"id":"behavior","target":"host","file":"main.wasm","provides":["character"]}]}).to_string().as_bytes());
    }
    #[test]
    fn acp_entrypoints_require_a_command_and_no_code_file() {
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("package");
        directory(&root);
        let write = |entry: serde_json::Value| {
            write_file(
                &root.join("manifest.json"),
                serde_json::json!({
                    "contract_version": ene_plugin::CONTRACT_VERSION,
                    "id": "acp", "name": "ACP", "version": "1",
                    "entrypoints": [entry]
                })
                .to_string()
                .as_bytes(),
            );
        };
        write(serde_json::json!({
            "id": "main", "target": "acp", "provides": ["harness"],
            "command": ["/bin/agent", "--acp"]
        }));
        Package::load(&root).unwrap();
        write(serde_json::json!({
            "id": "main", "target": "acp", "provides": ["harness"]
        }));
        assert!(Package::load(&root).is_err());
        write(serde_json::json!({
            "id": "main", "target": "acp", "file": "main.wasm",
            "provides": ["harness"], "command": ["/bin/agent"]
        }));
        assert!(Package::load(&root).is_err());
        write(serde_json::json!({
            "id": "main", "target": "acp", "provides": ["character"],
            "command": ["/bin/agent"]
        }));
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
        write_file(&path, serde_json::to_vec(&manifest).unwrap());
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
            write_file(&path, serde_json::to_vec(&manifest).unwrap());
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
        std::fs::hard_link(
            source_root.join("manifest.json"),
            source_root.join("main.wasm"),
        )
        .unwrap();
        assert!(Package::load(&source_root).is_err());
        #[cfg(unix)]
        {
            std::fs::remove_file(source_root.join("main.wasm")).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", source_root.join("main.wasm")).unwrap();
            assert!(Package::load(&source_root).is_err());
        }
    }
    #[cfg(unix)]
    #[test]
    fn packages_in_public_directories_are_rejected() {
        use std::os::unix::fs::PermissionsExt as _;
        let source = tempfile::tempdir().unwrap();
        let root = source.path().join("package");
        fixture(&root);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(Package::load(&root), Err(Error::Invalid)));
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(
            root.join("main.wasm"),
            std::fs::Permissions::from_mode(0o666),
        )
        .unwrap();
        assert!(matches!(Package::load(&root), Err(Error::Invalid)));
    }
}
