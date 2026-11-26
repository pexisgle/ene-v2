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
