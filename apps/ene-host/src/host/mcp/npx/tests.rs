use super::*;

#[test]
fn recognizes_npx_variants_and_rejects_unrelated_commands() {
    for cmd in [
        "npx",
        "NPX",
        "npx.cmd",
        "NPX.CMD",
        "npx.ps1",
        "npx.exe",
        r"C:\Program Files\nodejs\npx.cmd",
        r"C:\Program Files\nodejs\npx.ps1",
        r"D:/tools/node/npx",
        "  npx.cmd  ",
    ] {
        assert!(is_npx_command(cmd), "expected `{cmd}` to be treated as npx");
    }
    for cmd in ["node", "npm", "npx2", "my-npx", "cmd", "python3", ""] {
        assert!(
            !is_npx_command(cmd),
            "expected `{cmd}` not to be treated as npx"
        );
    }
}

#[cfg(windows)]
mod windows_tests {
    use super::*;
    use std::fs;

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"").unwrap();
    }

    #[tokio::test]
    async fn resolves_node_and_npx_cli_even_when_npx_cmd_is_absent_or_present() {
        let root = tempfile::tempdir().unwrap();
        let node_dir = root.path().join("nodejs");
        let node_exe = node_dir.join("node.exe");
        let cli_js = npm_npx_cli_in(&node_dir);
        touch(&node_exe);
        touch(&cli_js);

        // Works even when npx.cmd does not exist at all.
        let launch = resolve_launch(
            "npx",
            std::slice::from_ref(&node_dir),
            None,
            "",
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            launch,
            NpxLaunch::NodeCli {
                node_exe: node_exe.clone(),
                npx_cli_js: cli_js.clone(),
            }
        );

        // Still prefers node.exe + npx-cli.js even when npx.cmd is present.
        touch(&node_dir.join("npx.cmd"));
        let launch = resolve_launch(
            "npx.cmd",
            std::slice::from_ref(&node_dir),
            None,
            "",
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            launch,
            NpxLaunch::NodeCli {
                node_exe,
                npx_cli_js: cli_js,
            }
        );
    }

    #[tokio::test]
    async fn prefers_explicit_directory_over_path_and_supports_appdata_fallback() {
        let root = tempfile::tempdir().unwrap();
        let path_node_dir = root.path().join("path-node");
        touch(&path_node_dir.join("node.exe"));
        touch(&npm_npx_cli_in(&path_node_dir));

        let custom_dir = root.path().join("custom-node");
        let custom_node = custom_dir.join("node.exe");
        let custom_cli = npm_npx_cli_in(&custom_dir);
        touch(&custom_node);
        touch(&custom_cli);

        let explicit_cmd = custom_dir.join("npx.ps1");
        let launch = resolve_launch(
            explicit_cmd.to_str().unwrap(),
            std::slice::from_ref(&path_node_dir),
            None,
            "",
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            launch,
            NpxLaunch::NodeCli {
                node_exe: custom_node,
                npx_cli_js: custom_cli,
            }
        );

        // Node without bundled npm falls back to %APPDATA%\npm\node_modules\npm\bin\npx-cli.js.
        let bare_node_dir = root.path().join("bare-node");
        let bare_node = bare_node_dir.join("node.exe");
        touch(&bare_node);
        let appdata = root.path().join("appdata");
        let appdata_cli = npm_npx_cli_in(&appdata.join("npm"));
        touch(&appdata_cli);

        let launch = resolve_launch(
            "npx",
            std::slice::from_ref(&bare_node_dir),
            Some(&appdata),
            "",
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            launch,
            NpxLaunch::NodeCli {
                node_exe: bare_node,
                npx_cli_js: appdata_cli,
            }
        );
    }

    #[tokio::test]
    async fn falls_back_to_native_npx_exe_before_npx_cmd_and_errors_when_missing() {
        let root = tempfile::tempdir().unwrap();
        let bin_dir = root.path().join("bin");
        let npx_exe = bin_dir.join("npx.exe");
        let npx_cmd = bin_dir.join("npx.cmd");
        touch(&npx_exe);
        touch(&npx_cmd);

        let launch = resolve_launch(
            "npx",
            std::slice::from_ref(&bin_dir),
            None,
            "",
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(launch, NpxLaunch::DirectExecutable(npx_exe.clone()));

        fs::remove_file(&npx_exe).unwrap();
        let launch = resolve_launch(
            "npx",
            std::slice::from_ref(&bin_dir),
            None,
            "",
            &BTreeMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(launch, NpxLaunch::DirectExecutable(npx_cmd));

        let empty_dir = root.path().join("empty");
        fs::create_dir_all(&empty_dir).unwrap();
        let err = resolve_launch("npx", &[empty_dir], None, "", &BTreeMap::new())
            .await
            .unwrap_err();
        assert!(err.config_error);
    }

    #[tokio::test]
    async fn live_npx_runs_local_mcp_package_without_cmd_exe_or_npx_cmd() {
        // Requires Node.js + npm on the test host; skip only if absent.
        let host_path = std::env::var_os("PATH").unwrap_or_default();
        let host_dirs: Vec<PathBuf> = std::env::split_paths(&host_path).collect();
        let Some(host_node) = find_file_in_dirs(&host_dirs, "node.exe") else {
            return;
        };
        let Some(node_dir) = host_node.parent() else {
            return;
        };
        if !npm_npx_cli_in(node_dir).is_file() {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        // Local package with a Node bin and a cmd-shim `.cmd` wrapper under
        // `node_modules/.bin`, mimicking what `npm exec` / `npx` runs.
        let project = temp.path().join("project");
        let pkg_dir = project.join("node_modules").join("fake-mcp-server");
        let bin_dir = project.join("node_modules").join(".bin");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();

        fs::write(
            project.join("package.json"),
            r#"{"name":"host-test","version":"1.0.0"}"#,
        )
        .unwrap();
        fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"fake-mcp-server","version":"1.0.0","bin":{"fake-mcp-server":"cli.js"}}"#,
        )
        .unwrap();
        fs::write(
            pkg_dir.join("cli.js"),
            "#!/usr/bin/env node\nprocess.stdout.write(JSON.stringify({ args: process.argv.slice(2), marker: process.env.ENE_TEST_MARKER || '' }));\n",
        )
        .unwrap();
        // Standard npm cmd-shim wrapper + POSIX shim in node_modules/.bin.
        fs::write(
            bin_dir.join("fake-mcp-server.cmd"),
            "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n@SET ENE_TEST_MARKER=from-shim\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\..\\fake-mcp-server\\cli.js\" %*\r\n",
        )
        .unwrap();

        // Disable cmd.exe completely by pointing ComSpec at a non-existent
        // executable and restricting PATH to only the node directory.
        let mut env = BTreeMap::new();
        env.insert("ComSpec".into(), r"C:\ene-blocked-cmd\cmd.exe".into());
        env.insert("COMSPEC".into(), r"C:\ene-blocked-cmd\cmd.exe".into());
        env.insert("PATH".into(), node_dir.to_string_lossy().into_owned());

        let want = Desired {
            revision: 1,
            credential_generation: None,
            name: "fake-mcp".into(),
            command: "npx.cmd".into(),
            args: vec![
                "--no-install".into(),
                "fake-mcp-server".into(),
                r"C:\My Tools\日本語 & space".into(),
                "--flag=a^b".into(),
            ],
            cwd: project.to_string_lossy().into_owned(),
            env: env.clone(),
            headers: BTreeMap::new(),
            auth_header: String::new(),
            secret_env_var: String::new(),
            endpoint: String::new(),
            secret: None,
            allowed: Vec::new(),
        };

        let mut cmd = build_command(&want).await.unwrap();
        cmd.current_dir(&project);
        cmd.envs(&env);
        let output = tokio::time::timeout(Duration::from_secs(20), cmd.output())
            .await
            .expect("npx execution should finish")
            .expect("npx execution should spawn without cmd.exe");

        assert!(
            output.status.success(),
            "expected success without cmd.exe, stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let parsed: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
                panic!(
                    "stdout must remain clean JSON for MCP stdio ({err}): {:?}",
                    String::from_utf8_lossy(&output.stdout)
                )
            });
        assert_eq!(
            parsed["args"],
            serde_json::json!([r"C:\My Tools\日本語 & space", "--flag=a^b"])
        );
        assert_eq!(parsed["marker"], "from-shim");
    }

    #[tokio::test]
    async fn live_supervisor_open_connects_npx_mcp_server_without_cmd_exe() {
        let host_path = std::env::var_os("PATH").unwrap_or_default();
        let host_dirs: Vec<PathBuf> = std::env::split_paths(&host_path).collect();
        let Some(host_node) = find_file_in_dirs(&host_dirs, "node.exe") else {
            return;
        };
        let Some(node_dir) = host_node.parent() else {
            return;
        };
        if !npm_npx_cli_in(node_dir).is_file() {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let pkg_dir = project.join("node_modules").join("mini-mcp");
        let bin_dir = project.join("node_modules").join(".bin");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::create_dir_all(&bin_dir).unwrap();

        fs::write(
            project.join("package.json"),
            r#"{"name":"mcp-e2e","version":"1.0.0"}"#,
        )
        .unwrap();
        fs::write(
            pkg_dir.join("package.json"),
            r#"{"name":"mini-mcp","version":"1.0.0","bin":{"mini-mcp":"server.js"}}"#,
        )
        .unwrap();
        // Minimal JSON-RPC 2.0 stdio MCP server responding to initialize and tools/list.
        fs::write(
            pkg_dir.join("server.js"),
            r#"#!/usr/bin/env node
const readline = require('readline');
const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
rl.on('line', (line) => {
  if (!line.trim()) return;
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  if (msg.id === undefined || msg.id === null) return;
  let result = {};
  if (msg.method === 'initialize') {
result = {
  protocolVersion: (msg.params && msg.params.protocolVersion) || '2025-03-26',
  capabilities: { tools: {} },
  serverInfo: { name: 'mini-mcp', version: '1.0.0' },
};
  } else if (msg.method === 'tools/list') {
result = {
  tools: [{
    name: 'ping',
    description: 'Ping without cmd.exe',
    inputSchema: { type: 'object', properties: {} },
  }],
};
  }
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result }) + '\n');
});
"#,
        )
        .unwrap();
        fs::write(
            bin_dir.join("mini-mcp.cmd"),
            "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\..\\mini-mcp\\server.js\" %*\r\n",
        )
        .unwrap();

        let mut env = BTreeMap::new();
        env.insert("ComSpec".into(), r"C:\ene-blocked-cmd\cmd.exe".into());
        env.insert("COMSPEC".into(), r"C:\ene-blocked-cmd\cmd.exe".into());
        env.insert("PATH".into(), node_dir.to_string_lossy().into_owned());

        let want = Desired {
            revision: 1,
            credential_generation: None,
            name: "mini".into(),
            command: "npx".into(),
            args: vec!["--no-install".into(), "mini-mcp".into()],
            cwd: project.to_string_lossy().into_owned(),
            env,
            headers: BTreeMap::new(),
            auth_header: String::new(),
            secret_env_var: String::new(),
            endpoint: String::new(),
            secret: None,
            allowed: vec!["ping".into()],
        };

        let registry = rig_agent::tool::server::ToolServer::new().run();
        let connection = tokio::time::timeout(
            Duration::from_secs(20),
            crate::host::mcp::supervisor::open(&want, registry.clone()),
        )
        .await
        .expect("MCP open should complete within timeout")
        .expect("MCP open via npx should succeed without cmd.exe");

        let tools: Vec<String> = registry
            .snapshot()
            .definitions()
            .iter()
            .map(|def| def.name.as_str().to_owned())
            .collect();
        assert_eq!(tools, vec!["ping"]);
        connection.close().await;
    }
}
