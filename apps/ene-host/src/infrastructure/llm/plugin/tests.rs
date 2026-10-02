use super::*;
use ene_core::{
    domain::{Connection, Error, ErrorKind},
    ports::{Result, llm::*},
};
use ene_protocol::provider::PluginMessage;
use futures_util::{StreamExt, future::BoxFuture};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
fn bundle(program: &str) -> (tempfile::TempDir, manifest::Bundle) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("provider.py"), program).unwrap();
    let value = json!({"protocol_version":1,"id":"fixture","name":"Fixture","version":"1","command":["/usr/bin/python3","-I","-u","/plugin/provider.py"],"endpoint":"https://example.com/api","options":{},"auth":{"type":"none"},"destinations":["https://example.com"]});
    std::fs::write(dir.path().join("manifest.json"), value.to_string()).unwrap();
    let b = manifest::load(dir.path()).unwrap();
    (dir, b)
}
const INIT: &str = "import sys,json,os,socket\nassert not os.path.exists('/workspace')\nassert 'OPENAI_API_KEY' not in os.environ\nassert 'OPENROUTER_API_KEY' not in os.environ\nassert 'DBUS_SESSION_BUS_ADDRESS' not in os.environ\nm=json.loads(sys.stdin.readline())\nprint(json.dumps({'type':'initialized','request_id':m['request_id'],'protocol_version':1,'capabilities':{'streaming_text':True,'model_list':False}}),flush=True)\n";
struct Permit(AtomicBool);
impl Authorization for Permit {
    fn check(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            if self.0.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(Error::new(ErrorKind::PermissionRequired))
            }
        })
    }
}
fn invocation(
    authorization: Arc<dyn Authorization>,
) -> (tokio::sync::watch::Sender<bool>, Invocation) {
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    (
        cancel,
        Invocation {
            id: "generation".into(),
            connection: Connection {
                id: "c".into(),
                kind: "llm".into(),
                adapter: "fixture".into(),
                endpoint: "https://example.com/api".into(),
                options: json!({}),
                credential_state: "missing".into(),
                capabilities: None,
                last_check: None,
            },
            request: Generation {
                endpoint: "https://example.com/api".into(),
                options: json!({}),
                model: "model".into(),
                preamble: String::new(),
                messages: vec![],
                max_tokens: 128,
            },
            secret: None,
            authorization,
            cancel: receiver,
        },
    )
}
#[test]
fn contracts_configuration_and_implementation_changes_are_rejected() {
    let (dir, b) = bundle("");
    let before = b.revision;
    std::fs::write(dir.path().join("provider.py"), "changed").unwrap();
    assert_ne!(before, manifest::load(dir.path()).unwrap().revision);
    let path = dir.path().join("manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["protocol_version"] = json!(99);
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(manifest::load(dir.path()).is_err());
    value["protocol_version"] = json!(1);
    value["options"] = json!({"unknown":true});
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(manifest::load(dir.path()).is_err());
    value["options"] = json!({});
    value["id"] = json!("openai");
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(manifest::load(dir.path()).is_err());
    value["id"] = json!("openrouter");
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(manifest::load(dir.path()).is_ok());
    std::os::unix::fs::symlink("/etc/passwd", dir.path().join("link")).unwrap();
    assert!(manifest::load(dir.path()).is_err());
}
#[tokio::test]
async fn broker_rejects_revoked_context_redirect_origins_and_credential_headers() {
    let (_dir, b) = bundle("");
    let permit = Arc::new(Permit(AtomicBool::new(false)));
    let (_cancel, input) = invocation(permit.clone());
    let input = Arc::new(input);
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    let request = PluginMessage::HttpRequest {
        request_id: input.id.clone(),
        http_id: "http".into(),
        method: "GET".into(),
        url: "https://example.com/api/models".into(),
        headers: Default::default(),
        body_base64: None,
    };
    assert_eq!(
        broker::send(b.clone(), input.clone(), request.clone(), tx.clone())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::PermissionRequired
    );
    permit.0.store(true, Ordering::SeqCst);
    for (url, headers) in [
        ("https://other.com/api/models", Default::default()),
        ("https://example.com/elsewhere", Default::default()),
        (
            "https://example.com/api/models",
            [("authorization".into(), "Bearer injected".into())].into(),
        ),
    ] {
        let PluginMessage::HttpRequest {
            request_id,
            http_id,
            method,
            ..
        } = request.clone()
        else {
            unreachable!()
        };
        assert_eq!(
            broker::send(
                b.clone(),
                input.clone(),
                PluginMessage::HttpRequest {
                    request_id,
                    http_id,
                    method,
                    url: url.into(),
                    headers,
                    body_base64: None
                },
                tx.clone()
            )
            .await
            .unwrap_err()
            .kind,
            ErrorKind::PermissionRequired
        );
    }
}
#[tokio::test]
async fn isolated_plugin_streams_cancels_and_malformed_ipc_is_not_reused() {
    let program = format!(
        "{INIT}\nfor line in sys.stdin:\n m=json.loads(line)\n if m['type']=='generate':\n  print(json.dumps({{'type':'delta','request_id':m['request_id'],'text':'日本🙂'}}),flush=True)\n elif m['type']=='cancel':\n  print(json.dumps({{'type':'delta','request_id':m['request_id'],'text':'late'}}),flush=True)\n elif m['type']=='shutdown':break\n"
    );
    let (_dir, b) = bundle(&program);
    let probe = process::probe(&b).await;
    if !std::path::Path::new("/usr/bin/bwrap").exists() {
        assert!(probe.is_err());
        return;
    }
    probe.unwrap();
    let (cancel, input) = invocation(Arc::new(Permit(AtomicBool::new(true))));
    let mut stream = process::generate(b, input, process::Supervisor::default())
        .await
        .unwrap();
    assert!(matches!(stream.next().await.unwrap().unwrap(),Event::Delta(t) if t=="日本🙂"));
    cancel.send_replace(true);
    let event = tokio::time::timeout(std::time::Duration::from_secs(4), stream.next())
        .await
        .unwrap();
    assert!(event.is_none_or(|v| v.is_err()));
    assert!(stream.next().await.is_none());
    let (_dir, b) = bundle(&format!(
        "{INIT}\nfor line in sys.stdin:\n m=json.loads(line)\n if m['type']=='generate':print('not json',flush=True)\n elif m['type']=='shutdown':break\n"
    ));
    let (_cancel, input) = invocation(Arc::new(Permit(AtomicBool::new(true))));
    let mut stream = process::generate(b, input, process::Supervisor::default())
        .await
        .unwrap();
    assert!(stream.next().await.unwrap().is_err());
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn initialization_can_be_cancelled_before_handshake_completes() {
    if !std::path::Path::new("/usr/bin/bwrap").exists() {
        return;
    }
    let (_dir, bundle) = bundle(
        "import sys,json\njson.loads(sys.stdin.readline())\nfor line in sys.stdin:\n if json.loads(line)['type']=='shutdown':break\n",
    );
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    let start = std::time::Instant::now();
    let (result, ()) = tokio::join!(process::probe_with_cancel(&bundle, receiver), async {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.send_replace(true);
    });
    assert_eq!(result.unwrap_err().kind, ErrorKind::PermissionRequired);
    assert!(start.elapsed() < std::time::Duration::from_secs(4));
}
