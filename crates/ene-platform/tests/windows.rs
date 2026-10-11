#![cfg(windows)]
use ene_platform::{sandbox, storage};
use std::{io, path::Path};
use windows_sys::Win32::Security::Credentials::*;
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[test]
fn private_storage_rejects_public_acls_and_hardlinks_and_replaces_atomically() {
    let temp = tempfile::tempdir().unwrap();
    let public = temp.path().join("public");
    std::fs::write(&public, b"not private").unwrap();
    assert!(storage::read(&public, 1024, true).is_err());
    let root = temp.path().join("private");
    storage::create_private_dir_all(&root).unwrap();
    let path = root.join("connection.json");
    storage::write_atomic(&path, b"old").unwrap();
    storage::write_atomic(&path, "日本語🙂".as_bytes()).unwrap();
    assert_eq!(
        storage::read(&path, 1024, true).unwrap(),
        "日本語🙂".as_bytes()
    );
    std::fs::hard_link(&path, root.join("link")).unwrap();
    assert!(storage::read(&path, 1024, true).is_err());
    assert!(storage::write_atomic(&path, b"bad").is_err());
    std::fs::remove_file(root.join("link")).unwrap();
    assert!(storage::read(&path, 1, true).is_err());
}

#[test]
#[ignore = "only launched by the AppContainer integration test"]
fn sandbox_child() {
    std::panic::set_hook(Box::new(|info| println!("sandbox failure: {info}")));
    use std::io::{Read, Write};
    let secret = std::fs::read_to_string("host-path.txt").unwrap();
    assert!(
        std::fs::read(secret.trim()).is_err(),
        "host state must not be readable"
    );
    assert!(std::env::var_os("OPENAI_API_KEY").is_none());
    if let Ok(other) = std::fs::read_to_string("other-worker.txt") {
        assert!(
            std::fs::read(other.trim()).is_err(),
            "other worker must be unreadable"
        );
    }
    if let Ok(name) = std::fs::read_to_string("credential.txt") {
        let mut credential = std::ptr::null_mut();
        assert_eq!(
            unsafe { CredReadW(wide(&name).as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) },
            0,
            "host credential store must be inaccessible"
        );
    }
    assert!(
        std::fs::write("host-path.txt", b"changed").is_err(),
        "package must be read only"
    );
    let temp = std::env::var_os("TEMP").unwrap();
    std::fs::write(Path::new(&temp).join("allowed"), b"ok")
        .unwrap_or_else(|e| panic!("sandbox temp {}: {e}", Path::new(&temp).display()));
    let addr = std::fs::read_to_string("endpoint.txt")
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200)).is_err(),
        "direct loopback networking must be denied"
    );
    let mut byte = [0_u8; 1];
    std::io::stdin().read_exact(&mut byte).unwrap();
    std::io::stdout().write_all(b"sandbox-ok\n").unwrap();
}

#[test]
#[ignore = "only launched by the process-tree integration test"]
fn tree_descendant() {
    std::thread::sleep(std::time::Duration::from_secs(60));
}

#[test]
#[ignore = "only launched by the process-tree integration test"]
fn tree_child() {
    // The outer test waits on both process handles after killing the Job.
    use std::io::{Read, Write};
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tree_descendant", "--ignored", "--nocapture"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let secret = std::env::temp_dir().join("private-worker");
    std::fs::write(&secret, b"worker-private").unwrap();
    println!(
        "tree:{}:{}:{}",
        std::process::id(),
        child.id(),
        secret.display()
    );
    std::io::stdout().flush().unwrap();
    let mut bytes = [0; 1];
    let _ = std::io::stdin().read(&mut bytes);
    std::thread::sleep(std::time::Duration::from_secs(60));
    let _ = child.wait();
}

#[tokio::test]
async fn job_cancellation_stops_descendants_and_workers_cannot_share_files_or_credentials()
-> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use windows_sys::Win32::{Foundation::*, System::Threading::*};
    let temp = tempfile::tempdir()?;
    let package = temp.path().join("package");
    storage::create_private_dir_all(&package)?;
    std::fs::copy(std::env::current_exe()?, package.join("probe.exe"))?;
    let command = |fixture: &str| {
        vec![
            "/plugin/probe.exe".into(),
            "--exact".into(),
            fixture.into(),
            "--ignored".into(),
            "--nocapture".into(),
        ]
    };
    let mut tree = sandbox::spawn(&package, &command("tree_child"), None)?;
    let input = tree.stdin.take().unwrap();
    let mut output = BufReader::new(tree.stdout.take().unwrap());
    let mut line = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        line.clear();
        if tokio::time::timeout_at(deadline, output.read_line(&mut line))
            .await
            .map_err(io::Error::other)??
            == 0
        {
            return Err(io::Error::other(
                "tree fixture exited before reporting descendants",
            ));
        }
        if line.starts_with("tree:") {
            break;
        }
    }
    let mut parts = line.trim().splitn(4, ':');
    parts.next();
    let parent: u32 = parts.next().unwrap().parse().unwrap();
    let descendant: u32 = parts.next().unwrap().parse().unwrap();
    let other = parts.next().unwrap();
    let handles = [parent, descendant].map(|pid| {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(!handle.is_null());
        unsafe { OwnedHandle::from_raw_handle(handle) }
    });
    storage::write_atomic(&package.join("other-worker.txt"), other.as_bytes())?;
    let host = temp.path().join("host/secret");
    storage::write_atomic(&host, b"host")?;
    storage::write_atomic(
        &package.join("host-path.txt"),
        host.to_string_lossy().as_bytes(),
    )?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    storage::write_atomic(
        &package.join("endpoint.txt"),
        listener.local_addr()?.to_string().as_bytes(),
    )?;
    struct Credential(Vec<u16>);
    impl Drop for Credential {
        fn drop(&mut self) {
            unsafe {
                CredDeleteW(self.0.as_ptr(), CRED_TYPE_GENERIC, 0);
            }
        }
    }
    let name = format!("ene-test-{}", uuid::Uuid::new_v4());
    let mut guard = Credential(wide(&name));
    let mut blob = b"synthetic-only".to_vec();
    let credential = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: guard.0.as_mut_ptr(),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_SESSION,
        ..unsafe { std::mem::zeroed() }
    };
    if unsafe { CredWriteW(&credential, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    storage::write_atomic(&package.join("credential.txt"), name.as_bytes())?;
    let mut probe = sandbox::spawn(&package, &command("sandbox_child"), None)?;
    probe.stdin.take().unwrap().write_all(b"x").await?;
    let mut result = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        probe.stdout.take().unwrap().read_to_end(&mut result),
    )
    .await
    .map_err(io::Error::other)??;
    assert!(
        probe.wait().await?.success(),
        "{}",
        String::from_utf8_lossy(&result)
    );
    tree.kill().await?;
    drop(input);
    drop(tree);
    for handle in handles {
        assert_eq!(
            unsafe { WaitForSingleObject(handle.as_raw_handle(), 2000) },
            WAIT_OBJECT_0
        );
    }
    assert!(
        !Path::new(other).exists(),
        "worker temporary resources must be removed"
    );
    drop(guard);
    Ok(())
}

#[tokio::test]
async fn isolated_executable_has_bounded_stdio_and_no_host_or_network_access() -> io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let temp = tempfile::tempdir()?;
    let package = temp.path().join("package");
    let host = temp.path().join("host");
    storage::create_private_dir_all(&package)?;
    storage::create_private_dir_all(&host)?;
    let secret = host.join("secret");
    storage::write_atomic(&secret, b"private")?;
    std::fs::copy(std::env::current_exe()?, package.join("probe.exe"))?;
    storage::write_atomic(
        &package.join("host-path.txt"),
        secret.to_string_lossy().as_bytes(),
    )?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    storage::write_atomic(
        &package.join("endpoint.txt"),
        listener.local_addr()?.to_string().as_bytes(),
    )?;
    let mut child = sandbox::spawn(
        &package,
        &[
            "/plugin/probe.exe".into(),
            "--exact".into(),
            "sandbox_child".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ],
        None,
    )?;
    child.stdin.take().unwrap().write_all(b"x").await?;
    let mut output = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        child.stdout.take().unwrap().read_to_end(&mut output),
    )
    .await
    .map_err(io::Error::other)??;
    let status = child.wait().await?;
    assert!(
        status.success(),
        "{status}: {}",
        String::from_utf8_lossy(&output)
    );
    assert!(String::from_utf8_lossy(&output).contains("sandbox-ok"));
    Ok(())
}
