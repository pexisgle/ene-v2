use ene_plugin::*;
use ene_plugin_runtime::{
    Package,
    instance::{DenyServices, Instance, Services, Supervisor},
    limits::{MAX_CHUNK, MAX_MESSAGE_BYTES, WASM_TABLE_ELEMENTS},
    service::{HttpBroker, HttpGrants},
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

fn directory(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}
fn write_file(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}
fn write(data: &str) -> String {
    let mut code = format!("i64.const {} call $alloc local.set $p\n", data.len());
    for (i, b) in data.bytes().enumerate() {
        code += &format!("local.get $p i64.const {i} i64.add i32.const {b} call $store\n");
    }
    code
}
fn wasm(body: &str) -> Vec<u8> {
    let empty = write("{}");
    wat::parse_str(format!(r#"(module
        (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
        (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
        (import "extism:host/env" "output_set" (func $output (param i64 i64)))
        (import "extism:host/env" "length" (func $length (param i64) (result i64)))
        (import "extism:host/user" "ene_service" (func $service (param i64) (result i64)))
        (import "extism:host/user" "ene_emit" (func $emit (param i64)))
        (func (export "ene_initialize") (result i32) (local $p i64) {empty} local.get $p i64.const 2 call $output i32.const 0)
        (func (export "ene_invoke") (result i32) (local $p i64) (local $i i32)
            {body} {empty} local.get $p i64.const 2 call $output i32.const 0))"#)).unwrap()
}
fn package(code: &[u8], target: Target, services: &[Service]) -> (Package, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    directory(&root);
    write_file(&root.join("main.wasm"), code);
    write_file(&root.join("manifest.json"), json!({"contract_version":CONTRACT_VERSION,"id":"test","name":"Test","version":"1","entrypoints":[{"id":"main","file":"main.wasm","target":target,"provides":["character","harness"],"uses":services}]}).to_string().as_bytes());
    (Package::load(&root).unwrap(), temp)
}
fn character() -> Operation {
    Operation::Character {
        target: CharacterTarget {
            device_id: "device".into(),
            character_id: "character".into(),
        },
        event: CharacterEvent::Activated,
    }
}
fn emit(text: &str) -> String {
    format!("{} local.get $p call $emit",write(&json!({"type":"character","target":{"device_id":"device","character_id":"character"},"action":{"type":"speak","text":text}}).to_string()))
}
#[tokio::test]
async fn return_limits_count_the_raw_bytes_before_decoding() {
    for size in [MAX_CHUNK, MAX_CHUNK + 1] {
        let body = format!(
            r#"
            i64.const {size} call $alloc local.set $p
            (loop $fill
                local.get $p local.get $i i64.extend_i32_u i64.add i32.const 32 call $store
                local.get $i i32.const 1 i32.add local.tee $i i32.const {size} i32.lt_u br_if $fill)
            local.get $p i32.const 123 call $store
            local.get $p i64.const 1 i64.add i32.const 125 call $store
            local.get $p i64.const {size} call $output i32.const 0 return
        "#
        );
        let (package, _temp) = package(&wasm(&body), Target::Host, &[]);
        let cancel = CancellationToken::new();
        let mut instance = Instance::load(package, "main".into(), 1, &cancel)
            .await
            .unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let result = instance
            .invoke(character(), Arc::new(DenyServices), tx, &cancel)
            .await;
        if size == MAX_CHUNK {
            assert_eq!(result.unwrap(), json!({}));
        } else {
            assert!(matches!(result, Err(ene_plugin_runtime::Error::Protocol)));
        }
    }
}

#[tokio::test]
async fn result_operations_decode_their_existing_typed_documents() {
    let operations = [
        json!({"type": "embed", "request": {"endpoint": "", "options": {}, "model": "m", "inputs": ["a"], "dimensions": 1}}),
        json!({"type": "synthesize", "request": {"endpoint": "", "options": {}, "model": "m", "text": "a", "voice": "v", "speed": 1.0, "format": "pcm16"}}),
        json!({"type": "transcribe", "request": {"endpoint": "", "options": {}, "model": "m", "pcm_base64": "", "sample_rate": 24000, "language": null}}),
    ];
    let documents = [
        json!({"vectors": [[1.0]]}),
        json!({"handle": "audio", "sample_rate": 24000, "format": "pcm16", "bytes": 2}),
        json!({"text": "雪"}),
    ];
    for (operation, document) in operations.into_iter().zip(documents) {
        for valid in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("plugin");
            directory(&root);
            let reply = if valid {
                document.clone()
            } else {
                json!({"wrong": true})
            }
            .to_string();
            let body = format!(
                "{} local.get $p local.get $p call $length call $output i32.const 0 return",
                write(&reply)
            );
            write_file(&root.join("main.wasm"), wasm(&body));
            write_file(&root.join("manifest.json"), json!({"contract_version": CONTRACT_VERSION, "id": "typed", "name": "Typed", "version": "1", "entrypoints": [{"id": "main", "target": "host", "file": "main.wasm", "provides": ["embedding", "tts", "stt"], "provider": {"id": "typed", "endpoint": "http://127.0.0.1:1", "auth": {"type": "none"}, "destinations": []}}]}).to_string());
            let cancel = CancellationToken::new();
            let mut instance =
                Instance::load(Package::load(&root).unwrap(), "main".into(), 1, &cancel)
                    .await
                    .unwrap();
            let (tx, _rx) = mpsc::channel(1);
            let result = instance
                .invoke(
                    serde_json::from_value(operation.clone()).unwrap(),
                    Arc::new(DenyServices),
                    tx,
                    &cancel,
                )
                .await;
            if valid {
                assert_eq!(result.unwrap(), document);
            } else {
                assert!(matches!(result, Err(ene_plugin_runtime::Error::Json(_))));
            }
        }
    }
}

#[tokio::test]
async fn service_budget_counts_received_json_bytes() {
    // Trailing whitespace is legal JSON that re-encoding would hide.
    let request = format!(
        r#"{{"type":"state_read","key":"visits"}}{}"#,
        " ".repeat(8 * 1024)
    );
    let code = wasm(&format!(
        "{} (loop $next local.get $p call $service drop local.get $i i32.const 1 i32.add local.tee $i i32.const 2100 i32.lt_u br_if $next)",
        write(&request)
    ));
    let (package, _temp) = package(&code, Target::Host, &[Service::State]);
    let cancel = CancellationToken::new();
    let mut instance = Instance::load(package, "main".into(), 1, &cancel)
        .await
        .unwrap();
    let (tx, _rx) = mpsc::channel(1);
    let result = instance
        .invoke(character(), Arc::new(DenyServices), tx, &cancel)
        .await;
    assert!(
        result.is_err(),
        "2100 requests of 8 KiB must exceed the 16 MiB service budget"
    );
}
#[tokio::test]
async fn embedded_host_instance_preserves_event_order_under_backpressure() {
    let code = wasm(&format!(
        "(loop $next {} {} local.get $i i32.const 1 i32.add local.tee $i i32.const 100 i32.lt_u br_if $next)",
        emit("a"),
        emit("b")
    ));
    let cancel = CancellationToken::new();
    // An ACP entrypoint is a subprocess, not a Wasm instance: load refuses it.
    let acp_temp = tempfile::tempdir().unwrap();
    let acp_root = acp_temp.path().join("plugin");
    directory(&acp_root);
    write_file(
        &acp_root.join("manifest.json"),
        json!({"contract_version":CONTRACT_VERSION,"id":"acp","name":"Acp","version":"1","entrypoints":[{"id":"main","target":"acp","command":["/bin/true"],"provides":["harness"]}]})
            .to_string()
            .as_bytes(),
    );
    let acp = Package::load(&acp_root).unwrap();
    assert!(
        Instance::load(acp, "main".into(), 1, &cancel)
            .await
            .is_err()
    );
    let (package, _temp) = package(&code, Target::Host, &[]);
    let mut instance = Instance::load(package, "main".into(), 1, &cancel)
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::channel(1);
    let run = instance.invoke(character(), Arc::new(DenyServices), tx, &cancel);
    let collect = async {
        for i in 0..200 {
            assert!(
                matches!(rx.recv().await,Some(Event::Character {action:CharacterAction::Speak{text},..}) if text==if i%2==0 {"a"} else {"b"})
            );
        }
        assert!(rx.recv().await.is_none());
    };
    let (result, ()) = tokio::join!(run, collect);
    assert_eq!(result.unwrap(), json!({}));
}
struct Waiting {
    entered: Notify,
    dropped: Arc<Notify>,
}
struct Dropped(Arc<Notify>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
impl Services for Waiting {
    fn check(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
    fn handle(&self, _: ServiceRequest) -> BoxFuture<'_, Result<Value, Failure>> {
        Box::pin(async {
            let _drop = Dropped(self.dropped.clone());
            self.entered.notify_one();
            std::future::pending().await
        })
    }
}
#[tokio::test]
async fn cancellation_and_dropped_futures_release_waiting_host_functions() {
    for abort in [false, true] {
        let code = wasm(&format!(
            "{} local.get $p call $service drop",
            write(r#"{"type":"state_read","key":"visits"}"#)
        ));
        let (package, _temp) = package(&code, Target::Host, &[Service::State]);
        let cancel = CancellationToken::new();
        let mut instance = Instance::load(package, "main".into(), 1, &cancel)
            .await
            .unwrap();
        let services = Arc::new(Waiting {
            entered: Notify::new(),
            dropped: Arc::new(Notify::new()),
        });
        let (tx, _rx) = mpsc::channel(1);
        let run_services = services.clone();
        let token = cancel.clone();
        let task =
            tokio::spawn(
                async move { instance.invoke(character(), run_services, tx, &token).await },
            );
        tokio::time::timeout(Duration::from_secs(5), services.entered.notified())
            .await
            .unwrap();
        if abort {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            cancel.cancel();
            assert!(task.await.unwrap().is_err());
        }
        tokio::time::timeout(Duration::from_secs(2), services.dropped.notified())
            .await
            .unwrap();
    }
}
struct Unexpected;
impl Services for Unexpected {
    fn check(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
    fn handle(&self, _: ServiceRequest) -> BoxFuture<'_, Result<Value, Failure>> {
        panic!("unauthorized service reached the application")
    }
}
#[tokio::test]
async fn undeclared_services_do_not_reach_the_application() {
    let body = format!(
        "{} local.get $p call $service drop",
        write(r#"{"type":"state_read","key":"x"}"#)
    );
    let (package, _temp) = package(&wasm(&body), Target::Host, &[]);
    let cancel = CancellationToken::new();
    let mut instance = Instance::load(package, "main".into(), 1, &cancel)
        .await
        .unwrap();
    let (tx, _rx) = mpsc::channel(1);
    instance
        .invoke(character(), Arc::new(Unexpected), tx, &cancel)
        .await
        .unwrap();
}
#[tokio::test]
async fn fuel_invalid_targets_and_oversized_payloads_are_rejected() {
    let bad_event = format!(
        "{} local.get $p call $emit",
        write(
            r#"{"type":"character","target":{"device_id":"other","character_id":"character"},"action":{"type":"speak","text":"wrong"}}"#
        )
    );
    for body in [
        "(loop $forever br $forever)".to_string(),
        format!(
            "{} local.get $p i64.const 2 call $output i32.const 7 return",
            write("{}")
        ),
        bad_event,
        format!("i64.const {} call $alloc call $emit", MAX_CHUNK + 1),
        format!(
            "i64.const {} call $alloc call $service drop",
            MAX_MESSAGE_BYTES + 1
        ),
    ] {
        let (package, _temp) = package(&wasm(&body), Target::Host, &[]);
        let cancel = CancellationToken::new();
        let mut instance = Instance::load(package, "main".into(), 1, &cancel)
            .await
            .unwrap();
        let (tx, _rx) = mpsc::channel(1);
        assert!(
            tokio::time::timeout(
                Duration::from_secs(5),
                instance.invoke(character(), Arc::new(Unexpected), tx, &cancel)
            )
            .await
            .unwrap()
            .is_err()
        );
        assert!(matches!(
            instance.initialize(&cancel).await,
            Err(ene_plugin_runtime::Error::Unavailable)
        ));
    }
}
#[tokio::test]
async fn guest_memory_is_bounded_and_wasi_is_unavailable() {
    let (package, _temp) = package(
        &wasm(&format!(
            "i64.const 134217728 call $alloc i64.eqz if else {} local.get $p i64.const 18 call $output i32.const 0 return end",
            write(r#"{"allocated":true}"#)
        )),
        Target::Host,
        &[],
    );
    let cancel = CancellationToken::new();
    let mut instance = Instance::load(package, "main".into(), 1, &cancel)
        .await
        .unwrap();
    let (tx, _rx) = mpsc::channel(1);
    // Extism either refuses allocation with a null pointer or traps at its memory limit.
    let result = instance
        .invoke(character(), Arc::new(DenyServices), tx, &cancel)
        .await;
    assert!(
        matches!(result, Ok(ref value) if *value == json!({}))
            || matches!(result, Err(ene_plugin_runtime::Error::Execution(_)))
    );
    let wasi=wat::parse_str(r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32))))"#).unwrap();
    let (package, _temp) = self::package(&wasi, Target::Host, &[]);
    assert!(
        Instance::load(package, "main".into(), 1, &cancel)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn table_limits_cover_initial_size_count_and_growth() {
    let grow = format!(
        r#"
        ref.null func i32.const {WASM_TABLE_ELEMENTS} table.grow i32.eqz if else unreachable end
        table.size i32.const {WASM_TABLE_ELEMENTS} i32.eq if else unreachable end
        ref.null func i32.const 1 table.grow i32.const -1 i32.eq if else unreachable end
    "#
    );
    for (tables, body, allowed) in [
        ("(table 10000000 funcref)", "", false),
        ("(table 0 funcref) (table 0 funcref)", "", false),
        ("(table 0 funcref)", grow.as_str(), true),
        ("(table 0 10000000 funcref)", grow.as_str(), true),
    ] {
        let empty = write("{}");
        let code = wat::parse_str(format!(
            r#"(module
            (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
            (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
            (import "extism:host/env" "output_set" (func $output (param i64 i64)))
            {tables}
            (func $ok (export "ene_initialize") (result i32) (local $p i64)
                {empty} local.get $p i64.const 2 call $output i32.const 0)
            (func (export "ene_invoke") (result i32) {body} call $ok))"#
        ))
        .unwrap();
        let (package, _temp) = package(&code, Target::Host, &[]);
        let cancel = CancellationToken::new();
        let result = Instance::load(package, "main".into(), 1, &cancel).await;
        if allowed {
            let mut instance = result.unwrap();
            let (tx, _rx) = mpsc::channel(1);
            instance
                .invoke(character(), Arc::new(DenyServices), tx, &cancel)
                .await
                .unwrap();
        } else {
            assert!(
                matches!(result, Err(ene_plugin_runtime::Error::Execution(_))),
                "{tables}"
            );
        }
    }
}

struct Revoking(std::sync::atomic::AtomicBool);
impl Services for Revoking {
    fn check(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async {
            if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                Err(Failure::new("revoked"))
            } else {
                Ok(())
            }
        })
    }
    fn handle(&self, _: ServiceRequest) -> BoxFuture<'_, Result<Value, Failure>> {
        Box::pin(async {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(json!("secret response"))
        })
    }
}
#[tokio::test]
async fn revocation_during_a_host_function_prevents_subsequent_guest_output() {
    let body = format!(
        "{} local.get $p call $service drop {}",
        write(r#"{"type":"state_read","key":"x"}"#),
        emit("must not escape")
    );
    let (package, _temp) = package(&wasm(&body), Target::Host, &[Service::State]);
    let cancel = CancellationToken::new();
    let mut instance = Instance::load(package, "main".into(), 1, &cancel)
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::channel(1);
    let error = instance
        .invoke(
            character(),
            Arc::new(Revoking(std::sync::atomic::AtomicBool::new(false))),
            tx,
            &cancel,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("revoked"), "{error:#}");
    assert!(rx.recv().await.is_none());
}

struct Tagged {
    tag: &'static str,
    seen: Arc<tokio::sync::Mutex<Vec<String>>>,
}

impl Services for Tagged {
    fn handle(
        &self,
        request: ServiceRequest,
    ) -> BoxFuture<'_, std::result::Result<Value, Failure>> {
        let seen = self.seen.clone();
        Box::pin(async move {
            if let ServiceRequest::StateRead { key } = request {
                seen.lock().await.push(key);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok(json!(self.tag))
        })
    }
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn concurrent_and_reused_instances_receive_their_own_services() {
    let code = |key| {
        wasm(&format!(
            "{} local.get $p call $service local.set $p local.get $p local.get $p call $length call $output i32.const 0 return",
            write(&json!({"type":"state_read","key":key}).to_string())
        ))
    };
    let (first_package, _first_temp) = package(&code("first-key"), Target::Host, &[Service::State]);
    let (second_package, _second_temp) =
        package(&code("second-key"), Target::Host, &[Service::State]);
    let cancel = CancellationToken::new();
    let mut first = Instance::load(first_package, "main".into(), 1, &cancel)
        .await
        .unwrap();
    let mut second = Instance::load(second_package, "main".into(), 2, &cancel)
        .await
        .unwrap();
    let first_seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let second_seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let (first_tx, _first_rx) = mpsc::channel(1);
    let (second_tx, _second_rx) = mpsc::channel(1);
    let (first_result, second_result) = tokio::join!(
        first.invoke(
            character(),
            Arc::new(Tagged {
                tag: "first",
                seen: first_seen.clone()
            }),
            first_tx,
            &cancel
        ),
        second.invoke(
            character(),
            Arc::new(Tagged {
                tag: "second",
                seen: second_seen.clone()
            }),
            second_tx,
            &cancel
        )
    );
    assert_eq!(first_result.unwrap(), json!({"Ok": "first"}));
    assert_eq!(second_result.unwrap(), json!({"Ok": "second"}));
    assert_eq!(*first_seen.lock().await, vec!["first-key"]);
    assert_eq!(*second_seen.lock().await, vec!["second-key"]);

    first.initialize(&cancel).await.unwrap();
    let (tx, _rx) = mpsc::channel(1);
    let reused_seen = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let result = first
        .invoke(
            character(),
            Arc::new(Tagged {
                tag: "reused",
                seen: reused_seen.clone(),
            }),
            tx,
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(result, json!({"Ok": "reused"}));
    assert_eq!(*reused_seen.lock().await, vec!["first-key"]);
    assert_eq!(*first_seen.lock().await, vec!["first-key"]);
}

#[tokio::test]
async fn supervisor_shutdown_waits_for_cleanup_and_refuses_new_tasks() {
    let supervisor = Supervisor::default();
    let entered = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let started = entered.clone();
    let finished = dropped.clone();
    supervisor
        .spawn(async move {
            let _drop = Dropped(finished);
            started.notify_one();
            std::future::pending::<()>().await;
        })
        .await
        .unwrap();
    supervisor
        .spawn(async { panic!("fixture task failure") })
        .await
        .unwrap();
    entered.notified().await;
    supervisor.shutdown().await;
    tokio::time::timeout(Duration::from_secs(1), dropped.notified())
        .await
        .unwrap();
    assert!(supervisor.spawn(async {}).await.is_err());
}

#[tokio::test]
#[ignore = "requires ENE_REFERENCE_WASM pointing to a freshly built ene-reference-wasm"]
async fn sdk_guest_preserves_typed_events_and_service_failures() {
    let code =
        std::fs::read(std::env::var_os("ENE_REFERENCE_WASM").expect("ENE_REFERENCE_WASM")).unwrap();
    let (package, _temp) = package(&code, Target::Host, &[Service::State]);
    let cancel = CancellationToken::new();
    let mut instance = Instance::load(package, "main".into(), 1, &cancel)
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let output = instance
        .invoke(
            Operation::Harness {
                input: HarnessInput::Start {
                    prompt: vec![text_block("inspect")],
                },
            },
            Arc::new(DenyServices),
            tx,
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(output, json!({}));
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
    {
        events.push(event);
    }
    assert_eq!(events.len(), 4);
    assert!(matches!(
        events.last(),
        Some(Event::Harness {
            event: HarnessEvent::RequestPermission { .. }
        })
    ));
    let (tx, _rx) = mpsc::channel(1);
    let failure = instance
        .invoke(character(), Arc::new(DenyServices), tx, &cancel)
        .await
        .unwrap_err();
    assert!(
        failure.to_string().contains("permission_required"),
        "{failure}"
    );
}

/// Grants that allow one origin, as a provider host would for its endpoint.
struct AllowUrl(String);
impl HttpGrants for AllowUrl {
    fn allows(&self, _: &str, url: &url::Url) -> bool {
        url.origin().ascii_serialization() == self.0
    }
    fn authorize(&self, _: &str, _: &mut reqwest::header::HeaderMap) -> Result<(), Failure> {
        Ok(())
    }
}

/// Services that serve HTTP through the broker.
struct Brokered {
    broker: Arc<HttpBroker>,
    provider: String,
}

impl Services for Brokered {
    fn handle(
        &self,
        request: ServiceRequest,
    ) -> BoxFuture<'_, std::result::Result<Value, Failure>> {
        let broker = self.broker.clone();
        let provider = self.provider.clone();
        Box::pin(async move {
            Ok(match request {
                ServiceRequest::HttpOpen { request } => {
                    json!(broker.open(&provider, request).await?)
                }
                ServiceRequest::HttpRead { handle } => json!(broker.read(&handle)?),
                ServiceRequest::HttpClose { handle } => {
                    broker.close(&handle)?;
                    json!({})
                }
                _ => return Err(Failure::new("undeclared_service")),
            })
        })
    }
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
}

/// A broker-routing fixture that reads its endpoint through host HTTP and
/// returns a fixed, valid EmbeddingResult independently of the HTTP reply.
fn embedding_package(url: &str) -> (Package, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    directory(&root);
    let body = format!(
        "{} local.get $p call $service drop {} local.get $p call $service drop {} local.get $p call $service drop {} local.get $p local.get $p call $length call $output i32.const 0 return",
        write(&json!({"type":"http_open","request":{"method":"POST","url":url,"headers":{},"body_base64":null}}).to_string()),
        write(&json!({"type":"http_read","handle":"http-1"}).to_string()),
        write(&json!({"type":"http_close","handle":"http-1"}).to_string()),
        write(&json!({"vectors": [[1.0]]}).to_string()),
    );
    write_file(&root.join("main.wasm"), wasm(&body));
    write_file(
        &root.join("manifest.json"),
        json!({
            "contract_version": CONTRACT_VERSION,
            "id": "embedding", "name": "Embedding", "version": "1",
            "entrypoints": [{
                "id": "main", "target": "host", "file": "main.wasm",
                "provides": ["embedding"], "uses": ["http"],
                "provider": {
                    "id": "local-embedding",
                    "endpoint": url,
                    "auth": {"type": "none"},
                    "destinations": [url],
                    "extensions": ["embedding"]
                }
            }]
        })
        .to_string()
        .as_bytes(),
    );
    (Package::load(&root).unwrap(), temp)
}

#[tokio::test]
async fn an_embedding_plugin_reaches_its_endpoint_through_the_broker() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let served = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                break;
            }
            head.push(byte[0]);
        }
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi")
            .await;
    });
    let url = format!("http://{address}");
    let (package, _temp) = embedding_package(&url);
    let cancel = CancellationToken::new();
    let mut instance = Instance::load(package, "main".into(), 1, &cancel)
        .await
        .unwrap();
    let broker = Arc::new(HttpBroker::new(Arc::new(AllowUrl(url.clone()))).unwrap());
    let (tx, _rx) = mpsc::channel(1);
    let output = instance
        .invoke(
            Operation::Embed {
                request: EmbeddingRequest {
                    endpoint: url,
                    options: json!({}),
                    model: "m".into(),
                    inputs: vec!["a".into()],
                    dimensions: 1,
                },
            },
            Arc::new(Brokered {
                broker,
                provider: "local-embedding".into(),
            }),
            tx,
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(output, json!({"vectors": [[1.0]]}));
    // A refused exchange leaves the endpoint untouched, so waiting on it would
    // hang. Bound the wait and report what actually happened.
    tokio::time::timeout(Duration::from_secs(5), served)
        .await
        .expect("the plugin never reached its endpoint")
        .unwrap();
}

/// A generation plugin: it emits two text deltas and a finish reason, as a
/// streaming provider does.
fn generation_package() -> (Package, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    directory(&root);
    let delta = |text: &str| write(&json!({"type":"delta","text":text}).to_string());
    let body = format!(
        "{} local.get $p call $emit {} local.get $p call $emit {} local.get $p call $emit {} local.get $p local.get $p call $length call $output i32.const 0 return",
        delta("こんに"),
        delta("ちは"),
        write(&json!({"type":"finished","reason":"stop"}).to_string()),
        write("{}"),
    );
    write_file(&root.join("main.wasm"), provider_wasm(&body));
    write_file(
        &root.join("manifest.json"),
        json!({
            "contract_version": CONTRACT_VERSION,
            "id": "local-llm", "name": "Local LLM", "version": "1",
            "entrypoints": [{
                "id": "main", "target": "host", "file": "main.wasm",
                "provides": ["llm"], "uses": ["http"],
                "provider": {
                    "id": "local-llm", "endpoint": "http://127.0.0.1:1",
                    "auth": {"type": "none"},
                    "destinations": ["http://127.0.0.1:1"],
                    "extensions": ["llm"]
                }
            }]
        })
        .to_string()
        .as_bytes(),
    );
    (Package::load(&root).unwrap(), temp)
}

#[tokio::test]
async fn initialization_decodes_llm_capabilities_as_a_typed_document() {
    let (_package, temp) = generation_package();
    let root = temp.path().join("plugin");
    write_file(
        &root.join("main.wasm"),
        provider_wasm_with("", &json!({"streaming_text": "yes"})),
    );
    let package = Package::load(&root).unwrap();
    let cancel = CancellationToken::new();
    assert!(matches!(
        Instance::load(package, "main".into(), 1, &cancel).await,
        Err(ene_plugin_runtime::Error::Json(_))
    ));
}

/// A model-listing plugin: it answers `ListModels` with its `Models` event.
fn listing_package() -> (Package, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    directory(&root);
    let models = json!({
        "type": "models",
        "models": [
            {"id": "local-small", "name": "Local Small"},
            {"id": "local-large", "name": "Local Large"}
        ]
    });
    let body = format!(
        "{} local.get $p call $emit {} local.get $p local.get $p call $length call $output i32.const 0 return",
        write(&models.to_string()),
        write("{}"),
    );
    write_file(&root.join("main.wasm"), provider_wasm(&body));
    write_file(
        &root.join("manifest.json"),
        json!({
            "contract_version": CONTRACT_VERSION,
            "id": "local-list", "name": "Local List", "version": "1",
            "entrypoints": [{
                "id": "main", "target": "host", "file": "main.wasm",
                "provides": ["llm"], "uses": ["http"],
                "provider": {
                    "id": "local-list", "endpoint": "http://127.0.0.1:1",
                    "auth": {"type": "none"},
                    "destinations": ["http://127.0.0.1:1"],
                    "extensions": ["llm"]
                }
            }]
        })
        .to_string()
        .as_bytes(),
    );
    (Package::load(&root).unwrap(), temp)
}

/// The events a generation produced, in order.
async fn generate(package: Package, operation: Operation) -> (Value, Vec<Event>) {
    let cancel = CancellationToken::new();
    let mut instance = Instance::load(package, "main".into(), 1, &cancel)
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let collect = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    let output = instance
        .invoke(operation, Arc::new(DenyServices), tx, &cancel)
        .await
        .unwrap();
    (output, collect.await.unwrap())
}

#[tokio::test]
async fn a_generation_plugin_streams_text_and_finishes() {
    let (package, _temp) = generation_package();
    let (output, events) = generate(
        package,
        Operation::Generate {
            request: GenerationRequest {
                endpoint: "http://127.0.0.1:1".into(),
                options: json!({}),
                model: "local".into(),
                preamble: "p".into(),
                messages: vec![],
                tools: vec![],
                max_tokens: 16,
            },
        },
    )
    .await;
    assert_eq!(output, json!({}));
    let text: String = events
        .iter()
        .filter_map(|event| match event {
            Event::Delta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "こんにちは", "the deltas arrive in order");
    assert!(
        matches!(events.last(), Some(Event::Finished { reason }) if reason == "stop"),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_listing_plugin_answers_with_its_models() {
    let (package, _temp) = listing_package();
    let (_, events) = generate(
        package,
        Operation::ListModels {
            endpoint: "http://127.0.0.1:1".into(),
            options: json!({}),
        },
    )
    .await;
    let models = events
        .into_iter()
        .find_map(|event| match event {
            Event::Models { models } => Some(models),
            _ => None,
        })
        .expect("the plugin listed its models");
    let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids, ["local-small", "local-large"]);
}

/// A provider module whose `ene_initialize` declares streaming text, as an
/// LLM provider must.
fn provider_wasm(body: &str) -> Vec<u8> {
    provider_wasm_with(
        body,
        &json!({"streaming_text": true, "model_list": true, "tool_calls": true}),
    )
}

/// A provider module declaring `capabilities`, whose `ene_invoke` runs `body`.
fn provider_wasm_with(body: &str, capabilities: &Value) -> Vec<u8> {
    let empty = write("{}");
    let declared = capabilities.to_string();
    let capabilities = write(&declared);
    wat::parse_str(format!(r#"(module
        (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
        (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
        (import "extism:host/env" "output_set" (func $output (param i64 i64)))
        (import "extism:host/env" "length" (func $length (param i64) (result i64)))
        (import "extism:host/user" "ene_service" (func $service (param i64) (result i64)))
        (import "extism:host/user" "ene_emit" (func $emit (param i64)))
        (func (export "ene_initialize") (result i32) (local $p i64) {capabilities} local.get $p i64.const {} call $output i32.const 0)
        (func (export "ene_invoke") (result i32) (local $p i64) (local $i i32)
            {body} {empty} local.get $p i64.const 2 call $output i32.const 0))"#, declared.len())).unwrap()
}

/// A plugin that reads its input: it asks for a tool call the first time, and
/// answers with text once the conversation carries that call's result.
fn tool_plugin() -> Vec<u8> {
    // The guest compares the input's length against a threshold rather than
    // parsing JSON in WAT: the first turn carries no result, the second does.
    let call = write(
        &json!({
            "type": "tool_call",
            "id": "call-1",
            "name": "lookup",
            "arguments": {"query": "weather"}
        })
        .to_string(),
    );
    let text = write(&json!({"type": "delta", "text": "晴れです"}).to_string());
    let finished = write(&json!({"type": "finished", "reason": "stop"}).to_string());
    wat::parse_str(format!(r#"(module
        (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
        (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
        (import "extism:host/env" "output_set" (func $output (param i64 i64)))
        (import "extism:host/env" "length" (func $length (param i64) (result i64)))
        (import "extism:host/env" "input_length" (func $input_length (result i64)))
        (import "extism:host/user" "ene_emit" (func $emit (param i64)))
        (func (export "ene_initialize") (result i32) (local $p i64)
            {caps} local.get $p i64.const {caps_len} call $output i32.const 0)
        (func (export "ene_invoke") (result i32) (local $p i64)
            (if (result i32) (i64.lt_u (call $input_length) (i64.const {threshold}))
                (then {call} local.get $p call $emit {finished} local.get $p call $emit i32.const 0)
                (else {text} local.get $p call $emit {finished} local.get $p call $emit i32.const 0))
            drop
            {empty} local.get $p i64.const 2 call $output i32.const 0))"#,
        caps = write(&json!({"streaming_text": true, "tool_calls": true}).to_string()),
        caps_len = json!({"streaming_text": true, "tool_calls": true}).to_string().len(),
        // A first turn is a short request; the turn that carries the result is
        // longer because the result is in it.
        threshold = 400,
        empty = write("{}"),
    ))
    .unwrap()
}

#[tokio::test]
async fn a_plugin_asks_for_a_tool_and_answers_once_it_has_the_result() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    directory(&root);
    write_file(&root.join("main.wasm"), tool_plugin());
    write_file(
        &root.join("manifest.json"),
        json!({
            "contract_version": CONTRACT_VERSION,
            "id": "tool-llm", "name": "Tool LLM", "version": "1",
            "entrypoints": [{
                "id": "main", "target": "host", "file": "main.wasm",
                "provides": ["llm"], "uses": ["http"],
                "provider": {
                    "id": "tool-llm", "endpoint": "http://127.0.0.1:1",
                    "auth": {"type": "none"},
                    "destinations": ["http://127.0.0.1:1"],
                    "extensions": ["llm"]
                }
            }]
        })
        .to_string()
        .as_bytes(),
    );
    let package = Package::load(&root).unwrap();
    let cancel = CancellationToken::new();
    let mut instance = Instance::load(package, "main".into(), 1, &cancel)
        .await
        .unwrap();
    instance.initialize(&cancel).await.unwrap();

    // First turn: the plugin asks for a call instead of answering.
    let tool = ToolSpec {
        name: "lookup".into(),
        description: "look up a fact".into(),
        parameters: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
    };
    let (tx, mut rx) = mpsc::channel(16);
    let collect = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    instance
        .invoke(
            Operation::Generate {
                request: GenerationRequest {
                    endpoint: "http://127.0.0.1:1".into(),
                    options: json!({}),
                    model: "local".into(),
                    preamble: String::new(),
                    messages: vec![ChatMessage::text("user", "天気は?")],
                    tools: vec![tool.clone()],
                    max_tokens: 64,
                },
            },
            Arc::new(DenyServices),
            tx,
            &cancel,
        )
        .await
        .unwrap();
    let first = collect.await.unwrap();
    let (id, name, arguments) = first
        .iter()
        .find_map(|event| match event {
            Event::ToolCall {
                id,
                name,
                arguments,
            } => Some((id.clone(), name.clone(), arguments.clone())),
            _ => None,
        })
        .expect("the plugin asked for a tool call");
    assert_eq!(id, "call-1");
    assert_eq!(name, "lookup");
    assert_eq!(arguments["query"], "weather");

    // Second turn: the host returns the result, and the plugin answers.
    let (tx, mut rx) = mpsc::channel(16);
    let collect = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    });
    instance
        .invoke(
            Operation::Generate {
                request: GenerationRequest {
                    endpoint: "http://127.0.0.1:1".into(),
                    options: json!({}),
                    model: "local".into(),
                    preamble: String::new(),
                    messages: vec![
                        ChatMessage::text("user", "天気は?"),
                        ChatMessage::ToolCalls {
                            calls: vec![ToolCallRequest {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: arguments.clone(),
                            }],
                        },
                        ChatMessage::ToolResult {
                            call_id: id,
                            name,
                            content: "晴れ、25度".into(),
                            is_error: false,
                        },
                    ],
                    tools: vec![tool],
                    max_tokens: 64,
                },
            },
            Arc::new(DenyServices),
            tx,
            &cancel,
        )
        .await
        .unwrap();
    let second = collect.await.unwrap();
    let text: String = second
        .iter()
        .filter_map(|event| match event {
            Event::Delta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "晴れです", "the second turn answers in text");
    assert!(
        matches!(second.last(), Some(Event::Finished { reason }) if reason == "stop"),
        "{second:?}"
    );
}
