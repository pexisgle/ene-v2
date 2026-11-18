use super::*;
use futures_util::future::BoxFuture;
use serde_json::json;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

pub(super) struct NativeExit {
    pub error: Option<String>,
    pub fuel_consumed: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
enum WaitSite {
    Precheck,
    ServiceCheck,
    Handle,
    ServicePostcheck,
    EventCheck,
    Postcheck,
    FullSend,
    Cpu,
}
#[derive(Clone, Copy, Debug)]
enum Stop {
    Timeout,
    Cancel,
    Drop,
    ReceiverClosed,
}
struct PendingServices {
    site: WaitSite,
    checks: AtomicUsize,
    entered: Notify,
    released: Arc<Notify>,
}
struct Released(Arc<Notify>);
impl Drop for Released {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}
impl PendingServices {
    async fn pending<T>(&self) -> T {
        let _released = Released(self.released.clone());
        self.entered.notify_one();
        std::future::pending().await
    }
}
impl Services for PendingServices {
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>> {
        Box::pin(async {
            let check = self.checks.fetch_add(1, Ordering::SeqCst) + 1;
            let pending = match self.site {
                WaitSite::Precheck => check == 1,
                WaitSite::ServiceCheck | WaitSite::EventCheck | WaitSite::Postcheck => check == 2,
                WaitSite::ServicePostcheck => check == 3,
                _ => false,
            };
            if pending {
                return self.pending().await;
            }
            if matches!(self.site, WaitSite::FullSend) && check == 2 {
                self.entered.notify_one();
            }
            Ok(())
        })
    }
    fn handle(&self, _: ServiceRequest) -> BoxFuture<'_, std::result::Result<Value, Failure>> {
        Box::pin(async {
            if matches!(self.site, WaitSite::Handle) {
                return self.pending().await;
            }
            Ok(Value::Null)
        })
    }
}
fn put(data: &str) -> String {
    let mut code = format!("i64.const {} call $alloc local.set $p\n", data.len());
    for (i, b) in data.bytes().enumerate() {
        code += &format!("local.get $p i64.const {i} i64.add i32.const {b} call $store\n");
    }
    code
}
fn target() -> CharacterTarget {
    CharacterTarget {
        device_id: "device".into(),
        character_id: "character".into(),
    }
}
fn operation() -> Operation {
    Operation::Character {
        target: target(),
        event: CharacterEvent::Activated,
    }
}
fn event(text: &str) -> Event {
    Event::Character {
        target: target(),
        action: CharacterAction::Speak { text: text.into() },
    }
}
fn fixture(site: WaitSite) -> (Package, tempfile::TempDir) {
    let empty = put("{}");
    let emit = format!(
        "{} local.get $p call $emit",
        put(&serde_json::to_string(&event("ready")).unwrap())
    );
    let body = match site {
        WaitSite::ServiceCheck | WaitSite::Handle | WaitSite::ServicePostcheck => format!(
            "{} local.get $p call $service drop {emit}",
            put(r#"{"type":"state_read","key":"x"}"#)
        ),
        WaitSite::EventCheck | WaitSite::FullSend => emit,
        WaitSite::Cpu => format!("{emit} (loop $spin br $spin)"),
        _ => String::new(),
    };
    let code = wat::parse_str(format!(
        r#"(module
        (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
        (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
        (import "extism:host/env" "output_set" (func $output (param i64 i64)))
        (import "extism:host/user" "ene_service" (func $service (param i64) (result i64)))
        (import "extism:host/user" "ene_emit" (func $emit (param i64)))
        (func (export "ene_initialize") (result i32) (local $p i64)
            {empty} local.get $p i64.const 2 call $output i32.const 0)
        (func (export "ene_invoke") (result i32) (local $p i64)
            {body} {empty} local.get $p i64.const 2 call $output i32.const 0))"#
    ))
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("plugin");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("main.wasm"), code).unwrap();
    std::fs::write(
        root.join("manifest.json"),
        json!({
            "contract_version": CONTRACT_VERSION, "id":"test", "name":"Test", "version":"1",
            "entrypoints":[{"id":"main","file":"main.wasm","target":"host",
                "provides":["character"],"uses":["state"]}]
        })
        .to_string(),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        for file in ["manifest.json", "main.wasm"] {
            std::fs::set_permissions(root.join(file), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
    }
    (Package::load(&root).unwrap(), temp)
}
async fn gate(site: WaitSite, stop: Stop) {
    let (package, _temp) = fixture(site);
    let cancel = CancellationToken::new();
    let mut instance = Instance::load(package, "main".into(), &cancel)
        .await
        .unwrap();
    let native_exit = if matches!(site, WaitSite::Cpu) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        instance.native_exit = Some(tx);
        Some(rx)
    } else {
        None
    };
    let permit = Arc::downgrade(&instance.permit);
    let services = Arc::new(PendingServices {
        site,
        checks: AtomicUsize::new(0),
        entered: Notify::new(),
        released: Arc::new(Notify::new()),
    });
    let native = Arc::downgrade(&services);
    let (tx, mut rx) = mpsc::channel(1);
    if matches!(site, WaitSite::FullSend) {
        tx.try_send(event("queued before invocation")).unwrap();
    }
    let mut run = Box::pin(instance.invoke(operation(), services.clone(), tx, &cancel));
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = &mut run => panic!("{site:?}/{stop:?} returned before readiness: {result:?}"),
            _ = async {
                if matches!(site, WaitSite::Cpu) {
                    assert_eq!(serde_json::to_value(rx.recv().await.unwrap()).unwrap(), serde_json::to_value(event("ready")).unwrap());
                } else {
                    services.entered.notified().await;
                }
            } => {}
        }
    }).await.unwrap();
    if matches!(site, WaitSite::FullSend | WaitSite::Cpu) {
        tokio::select! {
            result = &mut run => panic!("{site:?} did not remain pending: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }
    match stop {
        Stop::Drop => drop(run),
        Stop::Timeout => {
            tokio::time::pause();
            tokio::time::advance(EXECUTION_TIMEOUT + Duration::from_secs(1)).await;
            let result = run.await;
            tokio::time::resume();
            assert!(
                matches!(result, Err(Error::Timeout)),
                "{site:?}: {result:?}"
            );
        }
        Stop::Cancel | Stop::ReceiverClosed => {
            if matches!(stop, Stop::Cancel) {
                cancel.cancel();
            } else {
                rx.close();
            }
            let result = tokio::time::timeout(Duration::from_secs(2), run)
                .await
                .unwrap();
            assert!(
                matches!(result, Err(Error::Cancelled)),
                "{site:?}: {result:?}"
            );
        }
    }
    if let Some(native_exit) = native_exit {
        let exit = tokio::time::timeout(Duration::from_secs(2), native_exit)
            .await
            .expect("CPU native call did not exit after the guard stopped it")
            .expect("CPU native completion observer was dropped");
        let consumed = exit.fuel_consumed.expect("CPU fuel accounting is enabled");
        assert_eq!(
            exit.error.as_deref(),
            Some("timeout"),
            "{site:?}/{stop:?} native exit with {consumed}/{WASM_FUEL} fuel consumed"
        );
        assert!(
            consumed < WASM_FUEL,
            "{site:?}/{stop:?} exhausted fuel instead of being interrupted"
        );
        eprintln!(
            "native {site:?}/{stop:?}: SDK timeout interrupt, fuel consumed {consumed}, fuel remaining {}",
            WASM_FUEL - consumed
        );
    }
    if !matches!(site, WaitSite::FullSend | WaitSite::Cpu) {
        tokio::time::timeout(Duration::from_secs(2), services.released.notified())
            .await
            .unwrap();
    }
    if matches!(site, WaitSite::FullSend) {
        assert_eq!(
            serde_json::to_value(rx.recv().await.unwrap()).unwrap(),
            serde_json::to_value(event("queued before invocation")).unwrap()
        );
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        assert!(
            rx.recv().await.is_none(),
            "{site:?}/{stop:?} emitted after stop"
        );
    })
    .await
    .unwrap();
    let (tx, _rx) = mpsc::channel(1);
    let reuse = instance
        .invoke(
            operation(),
            Arc::new(DenyServices),
            tx,
            &CancellationToken::new(),
        )
        .await;
    if matches!(site, WaitSite::Precheck) {
        assert!(
            reuse.is_ok(),
            "precheck must not enter or retire the guest: {reuse:?}"
        );
    } else {
        assert!(
            matches!(reuse, Err(Error::Unavailable)),
            "{site:?}/{stop:?} reused: {reuse:?}"
        );
    }
    drop(services);
    drop(instance);
    tokio::time::timeout(Duration::from_secs(2), async {
        while native.upgrade().is_some() || permit.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(SLOTS.clone().try_acquire_owned().is_ok());
    let guest = if matches!(site, WaitSite::Precheck) {
        "untouched"
    } else {
        "discarded"
    };
    eprintln!(
        "gate {site:?}/{stop:?}: classified, wait released, guest {guest}, native/slot released, no late output"
    );
}

const CALLBACKS: [WaitSite; 7] = [
    WaitSite::Precheck,
    WaitSite::ServiceCheck,
    WaitSite::Handle,
    WaitSite::ServicePostcheck,
    WaitSite::EventCheck,
    WaitSite::Postcheck,
    WaitSite::FullSend,
];
#[tokio::test]
async fn enclosing_timeouts_release_every_pending_wait() {
    for site in CALLBACKS {
        gate(site, Stop::Timeout).await;
    }
}
#[tokio::test]
async fn cancellation_releases_every_pending_wait() {
    for site in CALLBACKS {
        gate(site, Stop::Cancel).await;
    }
}
#[tokio::test]
async fn dropping_invocation_releases_every_pending_wait() {
    for site in CALLBACKS {
        gate(site, Stop::Drop).await;
    }
}
#[tokio::test]
async fn receiver_closure_releases_every_pending_wait() {
    for site in CALLBACKS {
        gate(site, Stop::ReceiverClosed).await;
    }
}
#[tokio::test]
async fn cpu_guest_is_interrupted_and_releases_its_native_slot() {
    for stop in [
        Stop::Timeout,
        Stop::Cancel,
        Stop::Drop,
        Stop::ReceiverClosed,
    ] {
        gate(WaitSite::Cpu, stop).await;
    }
}
#[tokio::test]
async fn dropping_a_pending_postcheck_discards_the_guest() {
    gate(WaitSite::Postcheck, Stop::Drop).await;
}

struct BlockingServices {
    entered: Notify,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl Services for BlockingServices {
    fn check(&self) -> BoxFuture<'_, std::result::Result<(), Failure>> {
        Box::pin(async { Ok(()) })
    }
    fn handle(&self, _: ServiceRequest) -> BoxFuture<'_, std::result::Result<Value, Failure>> {
        Box::pin(async {
            self.entered.notify_one();
            self.release.lock().unwrap().recv().unwrap();
            Ok(Value::Null)
        })
    }
}
struct Unblock(std::sync::mpsc::Sender<()>);
impl Drop for Unblock {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}
#[tokio::test]
async fn native_slot_remains_held_until_a_blocking_host_poll_returns() {
    let (package, _temp) = fixture(WaitSite::Handle);
    let cancel = CancellationToken::new();
    let mut instance = Instance::load(package, "main".into(), &cancel)
        .await
        .unwrap();
    let permit = Arc::downgrade(&instance.permit);
    let (release, blocked) = std::sync::mpsc::channel();
    let unblock = Unblock(release);
    let services = Arc::new(BlockingServices {
        entered: Notify::new(),
        release: Mutex::new(blocked),
    });
    let (tx, mut rx) = mpsc::channel(1);
    let mut run = Box::pin(instance.invoke(operation(), services.clone(), tx, &cancel));
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            result = &mut run => panic!("blocking host poll returned early: {result:?}"),
            _ = services.entered.notified() => {}
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    assert!(matches!(run.await, Err(Error::Cancelled)));
    drop(instance);
    assert!(
        permit.upgrade().is_some(),
        "native work lost its admission slot"
    );
    drop(unblock);
    tokio::time::timeout(Duration::from_secs(2), async {
        assert!(rx.recv().await.is_none());
        while permit.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(SLOTS.clone().try_acquire_owned().is_ok());
}
