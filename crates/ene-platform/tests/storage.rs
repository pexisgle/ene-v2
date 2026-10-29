use ene_platform::storage;
use std::io::Read;

#[test]
fn private_storage_rejects_public_permissions_and_hardlinks_and_replaces_atomically() {
    let temp = tempfile::tempdir().unwrap();
    let public = temp.path().join("public");
    std::fs::write(&public, b"not private").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    assert!(storage::open(&public, false, true).is_err());
    let root = temp.path().join("private");
    storage::create_private_dir_all(&root).unwrap();
    let path = root.join("connection.json");
    storage::write_atomic(&path, b"old").unwrap();
    storage::write_atomic(&path, "日本語🙂".as_bytes()).unwrap();
    let mut bytes = Vec::new();
    storage::open(&path, false, true)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, "日本語🙂".as_bytes());
    std::fs::hard_link(&path, root.join("link")).unwrap();
    assert!(storage::open(&path, false, true).is_err());
    assert!(storage::write_atomic(&path, b"bad").is_err());
    std::fs::remove_file(root.join("link")).unwrap();
    assert!(storage::open(&path, false, true).is_ok());
}
