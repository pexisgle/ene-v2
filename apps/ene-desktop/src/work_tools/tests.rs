#[cfg(unix)]
use super::helpers::image;
use super::{SHELL, workspace::Workspace};
use serde_json::{Value, json};
#[cfg(unix)]
use std::path::Path;
use std::sync::Arc;

#[tokio::test]
async fn workspace_tools_read_search_and_edit_only_the_bound_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Arc::new(Workspace::open(directory.path()).unwrap());
    let call = |name: &str, mut input: Value| {
        input["workspace"] = json!(workspace.root.to_string_lossy());
        let workspace = workspace.clone();
        let name = format!("work.{name}");
        async move { Box::pin(workspace.execute(&name, input)).await }
    };

    call(
        "write",
        json!({"path": "src/index.html", "content": "header\nhello\nfooter\n"}),
    )
    .await
    .unwrap();
    let read = call(
        "read",
        json!({"path": "src/index.html", "offset": 2, "limit": 1}),
    )
    .await
    .unwrap();
    assert_eq!(read["text"], "hello\n");
    assert_eq!(read["next_offset"], 3);
    let grep = call(
        "grep",
        json!({"pattern": "^HELLO$", "ignoreCase": true, "glob": "**/*.html"}),
    )
    .await
    .unwrap();
    assert_eq!(grep["entries"][0]["line"], 2);
    let find = call("find", json!({"pattern": "**/*.html"})).await.unwrap();
    assert_eq!(find["entries"].as_array().unwrap().len(), 1);
    let ls = call("ls", json!({})).await.unwrap();
    assert_eq!(ls["entries"][0], json!({"path": "src", "directory": true}));

    assert!(
        call(
            "edit",
            json!({"path": "src/index.html", "edits": [
                {"oldText": "hello", "newText": "world"},
                {"oldText": "missing", "newText": "oops"}
            ]})
        )
        .await
        .is_err()
    );
    let file = directory.path().join("src/index.html");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "header\nhello\nfooter\n"
    );
    call(
        "edit",
        json!({"path": "src/index.html", "edits": [{"oldText": "hello", "newText": "world"}]}),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "header\nworld\nfooter\n"
    );
    assert!(
        call("write", json!({"path": "../escape.html", "content": "bad"}))
            .await
            .is_err()
    );
    assert!(
        workspace
            .clone()
            .execute(
                "work.write",
                json!({
                    "workspace": "other", "path": "src/index.html", "content": "bad"
                })
            )
            .await
            .is_err()
    );

    if workspace.shell.is_some() {
        #[cfg(windows)]
        let command = "Get-Content -Raw src/index.html";
        #[cfg(not(windows))]
        let command = "cat src/index.html";
        let result = call(SHELL, json!({"command": command})).await.unwrap();
        assert_eq!(result["exit_code"], 0, "{result}");
        assert!(result["stdout"].as_str().unwrap().contains("world"));
    }
}

#[cfg(unix)]
#[test]
fn only_the_four_supported_images_are_treated_as_images() {
    // The old check was four magic-byte tests; `guess_format` must accept
    // exactly the same inputs and still reject the formats it recognises
    // but the image pipeline never received.
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    let jpeg = b"\xff\xd8\xff\xe0\0\x10JFIF";
    let gif87 = b"GIF87a\0\0";
    let gif89 = b"GIF89a\0\0";
    let mut webp = Vec::from(*b"RIFF\x1a\0\0\0WEBPVP8 ");
    webp.push(0);
    for (name, bytes) in [
        ("png", png.as_slice()),
        ("jpeg", jpeg.as_slice()),
        ("gif87", gif87.as_slice()),
        ("gif89", gif89.as_slice()),
        ("webp", webp.as_slice()),
    ] {
        assert!(image(bytes), "{name} must still be an image");
    }

    // Formats `guess_format` knows but this tool must not forward.
    let bmp = b"BM\x1a\0\0\0\0\0\0\0";
    let tiff = b"II*\0\x08\0\0\0";
    let ico = b"\0\0\x01\0\x01\0";
    for (name, bytes) in [
        ("bmp", bmp.as_slice()),
        ("tiff", tiff.as_slice()),
        ("ico", ico.as_slice()),
        ("plain text", b"hello".as_slice()),
        ("empty", b"".as_slice()),
    ] {
        assert!(!image(bytes), "{name} must not be treated as an image");
    }
}

#[cfg(unix)]
#[test]
fn file_operations_cannot_follow_a_link_out_of_the_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.path(), directory.path().join("link")).unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    assert!(workspace.load(Path::new("link/secret")).is_err());
    assert!(workspace.save(Path::new("link/new"), "bad", true).is_err());
    assert!(!outside.path().join("new").exists());
}
