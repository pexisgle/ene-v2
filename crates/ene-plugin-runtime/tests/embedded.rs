use ene_plugin::*;
use ene_plugin_runtime::{
    Package,
    instance::{DenyServices, Instance, Services},
    limits::MAX_CHUNK,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

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
        (import "extism:host/user" "ene_service" (func $service (param i64) (result i64)))
        (import "extism:host/user" "ene_emit" (func $emit (param i64)))
        (func (export "ene_initialize") (result i32) (local $p i64) {empty} local.get $p i64.const 2 call $output i32.const 0)
        (func (export "ene_invoke") (result i32) (local $p i64) (local $i i32)
            {body} {empty} local.get $p i64.const 2 call $output i32.const 0))"#)).unwrap()
}
fn package(code: &[u8], target: Target, services: &[Service]) -> (Package, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    ene_platform::storage::create_private_dir_all(&root).unwrap();
    ene_platform::storage::write_atomic(&root.join("main.wasm"), code).unwrap();
    ene_platform::storage::write_atomic(&root.join("manifest.json"), json!({"contract_version":CONTRACT_VERSION,"id":"test","name":"Test","version":"1","code_files":["main.wasm"],"entrypoints":[{"id":"main","file":"main.wasm","runtime":"extism","target":target,"provides":["character","harness","tts","stt"],"uses":services}]}).to_string().as_bytes()).unwrap();
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
async fn embedded_host_and_desktop_instances_preserve_event_order_under_backpressure() {
    for target in [Target::Host, Target::Desktop] {
        let code = wasm(&format!(
            "(loop $next {} {} local.get $i i32.const 1 i32.add local.tee $i i32.const 100 i32.lt_u br_if $next)",
            emit("a"),
            emit("b")
        ));
        let (package, _temp) = package(&code, target, &[]);
        let cancel = CancellationToken::new();
        let wrong = if target == Target::Host {
            Target::Desktop
        } else {
            Target::Host
        };
        assert!(
            Instance::load(package.clone(), "main".into(), wrong, 1, &cancel)
                .await
                .is_err()
        );
        let mut instance = Instance::load(package, "main".into(), target, 1, &cancel)
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
        let mut instance = Instance::load(package, "main".into(), Target::Host, 1, &cancel)
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
    let mut instance = Instance::load(package, "main".into(), Target::Host, 1, &cancel)
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
        "i32.const 7 return".to_string(),
        bad_event,
        format!("i64.const {} call $alloc call $emit", MAX_CHUNK+1),
        format!("{} local.get $p call $service drop",write(&json!({"type":"desktop","operation":{"type":"read_resource","handle":"x","offset":0,"length":MAX_CHUNK+1}}).to_string())),
    ] {
        let (package,_temp)=package(&wasm(&body),Target::Host,&[Service::Desktop]);
        let cancel=CancellationToken::new();
        let mut instance=Instance::load(package,"main".into(),Target::Host,1,&cancel).await.unwrap();
        let (tx,_rx)=mpsc::channel(1);
        assert!(tokio::time::timeout(Duration::from_secs(5),instance.invoke(character(),Arc::new(Unexpected),tx,&cancel)).await.unwrap().is_err());
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
    let mut instance = Instance::load(package, "main".into(), Target::Host, 1, &cancel)
        .await
        .unwrap();
    let (tx, _rx) = mpsc::channel(1);
    // Extism either refuses allocation with a null pointer or traps at its memory limit.
    let result = instance
        .invoke(character(), Arc::new(DenyServices), tx, &cancel)
        .await;
    assert!(
        matches!(result, Ok(ref value) if *value == json!({}))
            || matches!(result, Err(ene_plugin_runtime::Error::Execution))
    );
    let wasi=wat::parse_str(r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32))))"#).unwrap();
    let (package, _temp) = self::package(&wasi, Target::Host, &[]);
    assert!(
        Instance::load(package, "main".into(), Target::Host, 1, &cancel)
            .await
            .is_err()
    );
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
    let mut instance = Instance::load(package, "main".into(), Target::Host, 1, &cancel)
        .await
        .unwrap();
    let (tx, mut rx) = mpsc::channel(1);
    assert!(
        instance
            .invoke(
                character(),
                Arc::new(Revoking(std::sync::atomic::AtomicBool::new(false))),
                tx,
                &cancel
            )
            .await
            .is_err()
    );
    assert!(rx.recv().await.is_none());
}
