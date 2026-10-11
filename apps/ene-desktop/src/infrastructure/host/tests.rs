use super::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
#[test]
fn descriptor_rejects_public_symlink_and_remote_endpoints() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("connection.json");
    fs::write(
        &path,
        json!({"endpoint":"http://127.0.0.1:1234/rpc","token":"private"}).to_string(),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(descriptor(&path).is_ok());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(descriptor(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(descriptor(&link).is_err());
    for endpoint in [
        "http://remote:1234/rpc",
        "https://127.0.0.1:1234/rpc",
        "http://127.0.0.1:1234/rpc?token=bad",
        "http://user@127.0.0.1:1234/rpc",
    ] {
        fs::write(
            &path,
            json!({"endpoint":endpoint,"token":"private"}).to_string(),
        )
        .unwrap();
        assert!(descriptor(&path).is_err());
    }
}
