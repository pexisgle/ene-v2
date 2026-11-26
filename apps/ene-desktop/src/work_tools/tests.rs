use super::{
    OUTPUT_LIMIT, SHELL, WORKSPACE_FILE_CAPABILITIES, WorkToolError, workspace::Workspace,
};
use ene_domain::json::validate;
use serde_json::{Value, json};
#[cfg(unix)]
use std::path::Path;
use std::sync::Arc;

#[tokio::test]
async fn workspace_tools_read_search_and_edit_only_the_bound_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Arc::new(Workspace::open(directory.path()).unwrap());
    let specs = workspace.specs();
    assert_eq!(
        specs
            .iter()
            .take(WORKSPACE_FILE_CAPABILITIES.len())
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>(),
        WORKSPACE_FILE_CAPABILITIES,
    );
    let schema = |name: &str| -> Value {
        let name = format!("work.{name}");
        let spec = specs.iter().find(|spec| spec.name == name).unwrap();
        serde_json::from_str(&spec.input_schema).unwrap()
    };
    let root = workspace.root.to_string_lossy();
    for args in [
        json!({"workspace": "another workspace", "pattern": "hello"}),
        json!({"workspace": root, "pattern": "hello", "tool": "bash"}),
        json!({"workspace": root, "pattern": "hello", "glob": null}),
        json!({"workspace": root, "pattern": "hello", "limit": 501}),
    ] {
        assert!(validate(&schema("grep"), &args).is_err(), "{args}");
    }
    let call = |name: &str, mut input: Value| {
        input["workspace"] = json!(workspace.root.to_string_lossy());
        validate(&schema(name), &input).unwrap();
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

        #[cfg(windows)]
        let command = "[Console]::Out.Write('x' * 131072); [Console]::Error.Write('y' * 131072)";
        #[cfg(not(windows))]
        let command = "printf '%131072s' x; printf '%131072s' y >&2";
        let result = call(SHELL, json!({"command": command})).await.unwrap();
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["stdout"].as_str().unwrap().len(), OUTPUT_LIMIT);
        assert_eq!(result["stderr"].as_str().unwrap().len(), OUTPUT_LIMIT);
    }
}

#[tokio::test]
async fn read_and_grep_keep_the_first_unreturned_line_and_mark_truncated_text() {
    let directory = tempfile::tempdir().unwrap();
    let long_line = "日".repeat(OUTPUT_LIMIT / 3 + 1);
    std::fs::write(
        directory.path().join("book"),
        format!("head\n{long_line}\n日\n"),
    )
    .unwrap();
    let workspace = Arc::new(Workspace::open(directory.path()).unwrap());
    let call = |tool, mut input: Value| {
        input["workspace"] = json!(workspace.root.to_string_lossy());
        workspace.clone().execute(tool, input)
    };

    let first = call("work.read", json!({"path": "book"})).await.unwrap();
    assert_eq!(first["text"], "head\n");
    assert_eq!(first["next_offset"], 2);
    assert!(matches!(
        call("work.read", json!({"path": "book", "offset": 2})).await,
        Err(WorkToolError::LineTooLong)
    ));
    let grep = call(
        "work.grep",
        json!({"path": "book", "pattern": "日", "limit": 1}),
    )
    .await
    .unwrap();
    assert_eq!(grep["truncated"], true);
    assert_eq!(
        grep["entries"],
        json!([{"path":"book","line":2,"text":"日".repeat(2048 / 3)}])
    );
}

#[cfg(unix)]
#[tokio::test]
async fn file_operations_cannot_follow_a_link_out_of_the_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "secret").unwrap();
    std::os::unix::fs::symlink(outside.path(), directory.path().join("link")).unwrap();
    let workspace = Arc::new(Workspace::open(directory.path()).unwrap());
    assert!(workspace.load(Path::new("link/secret")).is_err());
    assert!(workspace.save(Path::new("link/new"), "bad", true).is_err());
    assert!(!outside.path().join("new").exists());
    let input = json!({"workspace": workspace.root.to_string_lossy(), "pattern": "secret"});
    let output = workspace
        .clone()
        .execute("work.grep", input.clone())
        .await
        .unwrap();
    assert_eq!(output["entries"], json!([]));
    let output = workspace.execute("work.find", input).await.unwrap();
    assert_eq!(output["entries"], json!([]));
}
