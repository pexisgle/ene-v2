//! Exercises the actual Python extension and Rust RTC broker across OS pipes.
use ene_ipc::{
    provider::{
        BrokerClient, Client, Codec, GenerationRequest, HttpBroker, HttpBrokerServerShared,
        HttpRequest, HttpResponsePart, Provider, ProviderError, Sender,
    },
    remoc::{self, ConnectExt, rch::mpsc, rtc::ServerShared},
};
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio_util::task::AbortOnDropHandle;

struct Broker;
impl HttpBroker for Broker {
    async fn request(
        &self,
        request: HttpRequest,
        response: Sender<HttpResponsePart>,
    ) -> Result<(), ProviderError> {
        if request.url == "fail" {
            return Err(ProviderError::PermissionDenied);
        }
        if request.url == "binary" {
            assert_eq!(request.method, "POST");
            assert_eq!(request.body.unwrap(), [0, 255, 10]);
        } else {
            assert_eq!(request.method, "GET");
            assert!(request.body.is_none());
        }
        response
            .send(HttpResponsePart::Head {
                status: 200,
                headers: [("content-type".into(), "text/plain".into())].into(),
            })
            .await
            .map_err(|_| ProviderError::Unavailable)?
            .await
            .map_err(|_| ProviderError::Unavailable)?;
        if request.url == "stall" {
            std::future::pending::<()>().await;
        }
        if request.url == "partial" {
            return Err(ProviderError::Failed);
        }
        // A multibyte UTF-8 character can span HTTP chunks.
        for chunk in "日本語".as_bytes().chunks(2) {
            response
                .send(HttpResponsePart::Data(chunk.to_vec()))
                .await
                .map_err(|_| ProviderError::Unavailable)?
                .await
                .map_err(|_| ProviderError::Unavailable)?;
        }
        Ok(())
    }
}
fn broker() -> (
    BrokerClient,
    AbortOnDropHandle<Result<(), remoc::rtc::ServeError>>,
) {
    let (server, client) = HttpBrokerServerShared::<_, Codec>::new(Arc::new(Broker));
    (client, AbortOnDropHandle::new(tokio::spawn(server.serve())))
}
fn request(model: &str) -> GenerationRequest {
    GenerationRequest {
        endpoint: "unused".into(),
        options: serde_json::json!({"nested":[true,null,{"text":"日本語"}]}),
        model: model.into(),
        preamble: "a".repeat(1_100_000),
        messages: vec![],
        max_tokens: 1 << 63,
    }
}

#[tokio::test]
async fn python_provider_calls_rust_broker_streams_and_cancels() {
    tokio::time::timeout(Duration::from_secs(20), exercise())
        .await
        .unwrap();
}
async fn exercise() {
    let image = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let target = exe
        .ancestors()
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name == "debug" || name == "release")
        })
        .unwrap();
    let library = if cfg!(windows) {
        "ene_provider.dll"
    } else {
        "libene_provider.so"
    };
    let mut candidates = vec![target.join(library), target.join("deps").join(library)];
    // The pinned Cargo's artifact layout places cdylibs under build/<package>.
    if let Ok(artifacts) = std::fs::read_dir(target.join("build/ene-python")) {
        candidates.extend(
            artifacts
                .flatten()
                .map(|entry| entry.path().join("out").join(library)),
        );
    }
    let extension = candidates
        .into_iter()
        .filter_map(|path| {
            let modified = path.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        })
        .max_by_key(|(modified, _)| *modified)
        .expect("Cargo must build the SDK cdylib")
        .1;
    std::fs::copy(
        extension,
        image.path().join(if cfg!(windows) {
            "ene_provider.pyd"
        } else {
            "ene_provider.so"
        }),
    )
    .unwrap();
    let marker = image.path().join("cancelled");
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/provider.py");
    let mut child = tokio::process::Command::new(if cfg!(windows) { "python" } else { "python3" })
        .arg("-I")
        .arg(script)
        .arg(image.path())
        .arg(&marker)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client: Client = remoc::Connect::io::<_, _, Client, Client, remoc::codec::Default>(
        remoc::Cfg::default(),
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
    )
    .consume()
    .await
    .unwrap();
    assert!(client.capabilities().await.unwrap().model_list);
    let (callback, _server) = broker();
    let models = client
        .list_models("test".into(), serde_json::json!({}), callback)
        .await
        .unwrap();
    assert_eq!(models[0].id, "日本語");
    assert_eq!(
        models[0].metadata.as_ref().unwrap().context_window,
        Some(1 << 63)
    );
    let (callback, _server) = broker();
    let (first, second) = tokio::join!(
        client.list_models("test".into(), serde_json::json!({}), callback.clone()),
        client.list_models("test".into(), serde_json::json!({}), callback),
    );
    assert_eq!(first.unwrap()[0].id, "日本語");
    assert_eq!(second.unwrap()[0].id, "日本語");
    for (endpoint, options) in [
        ("test", serde_json::json!({"fail":true})),
        ("fail", serde_json::json!({})),
        ("partial", serde_json::json!({})),
    ] {
        let (callback, _server) = broker();
        assert!(matches!(
            client.list_models(endpoint.into(), options, callback).await,
            Err(ProviderError::Failed)
        ));
    }
    let (callback, _server) = broker();
    let (sender, mut deltas) = mpsc::channel::<String, Codec>();
    let (result, text) = tokio::join!(
        client.generate(request("normal"), callback, sender),
        async {
            let mut text = String::new();
            while let Some(delta) = deltas.recv().await.unwrap() {
                text.push_str(&delta);
            }
            text
        }
    );
    assert_eq!(result.unwrap(), "stop");
    assert_eq!(text, "日本語".repeat(30000));
    let (callback, _server) = broker();
    let (sender, mut deltas) = mpsc::channel::<String, Codec>();
    let call_client = client.clone();
    let call = tokio::spawn(async move {
        call_client
            .generate(request("cancel"), callback, sender)
            .await
    });
    assert!(deltas.recv().await.unwrap().is_some());
    // Dropping the Rust RTC call must also cancel the Python coroutine.
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    // Creating the file precedes writing its bytes, especially on Windows.
    // Wait for the coroutine's completed marker, rather than its empty file.
    while !std::fs::read_to_string(&marker).is_ok_and(|text| text == "cancelled") {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(client.capabilities().await.unwrap().streaming_text);
    drop(deltas);
    drop(client);
    assert!(child.wait().await.unwrap().success());
}
