//! Packages are read without executing code. Executable files are captured and installed
//! into a private, read-only snapshot; large assets are not mounted into workers.
use crate::{Error, Result};
use ene_ipc::plugin::*;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
const MAX_CODE: u64 = 32 * 1024 * 1024;
#[derive(Clone, Debug)]
pub struct Package {
    pub root: PathBuf,
    pub manifest: Manifest,
    pub revision: String,
    files: Arc<BTreeMap<String, Vec<u8>>>,
}
#[derive(Clone, Debug)]
pub struct Installed {
    pub package: Package,
    pub root: PathBuf,
    _owner: Arc<SnapshotOwner>,
}
#[derive(Debug)]
struct SnapshotOwner(PathBuf);
impl Drop for SnapshotOwner {
    fn drop(&mut self) {
        fn writable(path: &Path) {
            if let Ok(items) = std::fs::read_dir(path) {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
                }
                for item in items.flatten() {
                    if item.path().is_dir() {
                        writable(&item.path());
                    }
                }
            }
        }
        writable(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
pub fn relative(path: &str) -> bool {
    ene_platform::relative(path)
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
fn read(root: &Path, path: &str, limit: u64, snapshot: bool) -> Result<Vec<u8>> {
    if !relative(path) {
        return Err(Error::Invalid);
    }
    let file = open_code(root, path, snapshot)?;
    let m = file.metadata()?;
    if !m.is_file() || m.len() > limit {
        return Err(Error::Invalid);
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(Error::Invalid);
    }
    Ok(bytes)
}
fn open_code(root: &Path, path: &str, snapshot: bool) -> Result<std::fs::File> {
    #[cfg(windows)]
    if snapshot {
        return Ok(ene_platform::storage::open_snapshot(root, Some(path))?);
    }
    let _ = snapshot;
    let mut parent = root.to_path_buf();
    let components: Vec<_> = Path::new(path).components().collect();
    for part in &components[..components.len() - 1] {
        parent.push(part);
        owned(&parent, true)?;
    }
    Ok(ene_platform::storage::open(&root.join(path), false, false)?)
}
impl Package {
    pub fn load(root: &Path) -> Result<Self> {
        Self::load_inner(root, true)
    }
    pub fn load_snapshot(root: &Path) -> Result<Self> {
        Self::load_inner(root, false)
    }
    fn load_inner(root: &Path, check_assets: bool) -> Result<Self> {
        #[cfg(windows)]
        if !check_assets {
            ene_platform::storage::open_snapshot(root, None)?;
        } else {
            owned(root, true)?;
        }
        #[cfg(unix)]
        owned(root, true)?;
        let raw = read(root, "manifest.json", 65536, !check_assets)?;
        let manifest: Manifest = serde_json::from_slice(&raw)?;
        if manifest.package_version != PACKAGE_VERSION
            || manifest.api_version != API_VERSION
            || !identifier(&manifest.id)
            || manifest.id == "openai"
            || manifest.name.trim().is_empty()
            || manifest.version.is_empty()
            || manifest.entrypoints.is_empty()
            || manifest.entrypoints.len() > 32
            || manifest.code_files.is_empty()
            || manifest.code_files.len() > 256
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
                || !manifest.code_files.contains(&e.file)
                || e.provides.is_empty()
                || e.provides.len() > 6
                || e.uses.len() > 8
                || e.runtime == Runtime::ProviderRemoc
                || e.provides.iter().collect::<BTreeSet<_>>().len() != e.provides.len()
                || e.uses.iter().collect::<BTreeSet<_>>().len() != e.uses.len()
            {
                return Err(Error::Invalid);
            }
            if e.runtime == Runtime::Process {
                if e.command.is_empty()
                    || e.command.len() > 32
                    || !e
                        .command
                        .iter()
                        .any(|a| a == &format!("/plugin/{}", e.file))
                    || !ene_platform::sandbox::command_valid(&e.command)
                    || e.command
                        .iter()
                        .any(|a| a.len() > 4096 || a.chars().any(char::is_control))
                {
                    return Err(Error::Invalid);
                }
            } else if !e.command.is_empty() {
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
        let mut portable_names = BTreeMap::new();
        portable_name(&mut portable_names, "manifest.json")?;
        let mut total = files["manifest.json"].len() as u64;
        for path in &manifest.code_files {
            if files.contains_key(path) {
                return Err(Error::Invalid);
            }
            portable_name(&mut portable_names, path)?;
            let data = read(root, path, MAX_CODE, !check_assets)?;
            total += data.len() as u64;
            if total > MAX_CODE {
                return Err(Error::Invalid);
            }
            files.insert(path.clone(), data);
        }
        for asset in manifest.assets.values() {
            portable_name(&mut portable_names, &asset.path)?;
            if files.contains_key(&asset.path)
                || asset.size > 4 * 1024 * 1024 * 1024
                || asset.sha256.len() != 64
                || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err(Error::Invalid);
            }
            if check_assets {
                let m = std::fs::symlink_metadata(root.join(&asset.path))?;
                if !m.is_file() || m.file_type().is_symlink() || m.len() != asset.size {
                    return Err(Error::Invalid);
                }
            }
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
            revision: format!("{:x}", hash.finalize()),
            files: Arc::new(files),
        })
    }
    pub fn verify(&self) -> Result<()> {
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
    pub fn install(&self, cache: &Path) -> Result<Installed> {
        self.verify()?;
        // Older Linux installations created the cache parent with mode 0755;
        // individual snapshots still get private directories and frozen code.
        if cache.exists() {
            owned(cache, true)?;
        } else {
            ene_platform::storage::create_private_dir_all(cache)?;
        }
        owned(cache, true)?;
        let root = cache.join(uuid::Uuid::new_v4().to_string());
        ene_platform::storage::create_private_dir_all(&root)?;
        let owner = Arc::new(SnapshotOwner(root.clone()));
        for (path, data) in self.files.iter() {
            let file = root.join(path);
            if let Some(parent) = file.parent() {
                ene_platform::storage::create_private_dir_all(parent)?;
            }
            ene_platform::storage::write_atomic(&file, data)?;
            ene_platform::storage::freeze(&file, false)?;
        }
        fn freeze(path: &Path) -> std::io::Result<()> {
            for item in std::fs::read_dir(path)? {
                let p = item?.path();
                if p.is_dir() {
                    freeze(&p)?;
                }
            }
            ene_platform::storage::freeze(path, true)
        }
        freeze(&root)?;
        // Verify the bytes that will actually be executed, not just their source names.
        if Self::load_snapshot(&root)?.revision != self.revision {
            return Err(Error::Changed);
        }
        Ok(Installed {
            package: self.clone(),
            root,
            _owner: owner,
        })
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
    use super::*;
    #[test]
    fn package_components_reject_case_aliases_and_file_directory_collisions() {
        let mut names = BTreeMap::new();
        portable_name(&mut names, "code/a.luau").unwrap();
        portable_name(&mut names, "code/b.luau").unwrap();
        for path in ["Code/c.luau", "code/A.luau", "code/a.luau/child", "code"] {
            assert!(portable_name(&mut names.clone(), path).is_err(), "{path}");
        }
    }
    fn fixture(dir: &Path) {
        ene_platform::storage::create_private_dir_all(dir).unwrap();
        ene_platform::storage::write_atomic(&dir.join("main.luau"), b"return {}").unwrap();
        ene_platform::storage::write_atomic(&dir.join("manifest.json"), serde_json::json!({"package_version":1,"api_version":1,"id":"sample","name":"Sample","version":"1","code_files":["main.luau"],"entrypoints":[{"id":"behavior","target":"host","runtime":"luau","file":"main.luau","provides":["character"]}]}).to_string().as_bytes()).unwrap();
    }
    #[test]
    fn snapshots_capture_code_and_source_changes_invalidate_revision() {
        let source = tempfile::tempdir().unwrap();
        let source_root = source.path().join("package");
        let cache_base = tempfile::tempdir().unwrap();
        let cache = cache_base.path().join("cache");
        fixture(&source_root);
        let package = Package::load(&source_root).unwrap();
        let installed = package.install(&cache).unwrap();
        std::fs::write(source_root.join("main.luau"), "return {changed=true}").unwrap();
        assert!(matches!(package.verify(), Err(Error::Changed)));
        assert_eq!(
            Package::load(&installed.root).unwrap().revision,
            package.revision
        );
        let path = installed.root.clone();
        drop(installed);
        assert!(!path.exists());
    }
    #[test]
    fn module_traversal_and_links_are_rejected() {
        assert!(!relative("../secret"));
        assert!(!relative("/secret"));
        assert!(!relative("a/./b"));
        let source = tempfile::tempdir().unwrap();
        let source_root = source.path().join("package");
        fixture(&source_root);
        std::fs::remove_file(source_root.join("main.luau")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", source_root.join("main.luau")).unwrap();
        #[cfg(windows)]
        std::fs::hard_link(
            source_root.join("manifest.json"),
            source_root.join("main.luau"),
        )
        .unwrap();
        assert!(Package::load(&source_root).is_err());
    }
}
