//! Exercise real runner binaries, including cancellation while a host callback waits.
use ene_ipc::plugin::*;
use ene_plugin_runtime::transport::{Reader, Writer};
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::process::{Child, Command};
struct Runner {
    child: Child,
    input: Writer<HostMessage>,
    output: Reader<WorkerMessage>,
    call: Call,
    _package: tempfile::TempDir,
}
impl Runner {
    async fn start(code: &[u8], runtime: Runtime) -> Self {
        let package = tempfile::tempdir().unwrap();
        let root = package.path().join("package");
        ene_platform::storage::create_private_dir_all(&root).unwrap();
        let file = if runtime == Runtime::Luau {
            "main.luau"
        } else {
            "main.wasm"
        };
        ene_platform::storage::write_atomic(&root.join(file), code).unwrap();
        ene_platform::storage::write_atomic(&root.join("manifest.json"),json!({"package_version":1,"api_version":1,"id":"test","name":"Test","version":"1","code_files":[file],"entrypoints":[{"id":"main","file":file,"runtime":runtime,"target":"host","provides":["character","harness","tts","stt"],"uses":["state","http"]}]}).to_string().as_bytes()).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_ene-plugin-runner"))
            .arg("--package")
            .arg(root)
            .args(["--entrypoint", "main"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (mut input, output) = ene_plugin_runtime::transport::host(
            child.stdout.take().unwrap(),
            child.stdin.take().unwrap(),
        )
        .await
        .unwrap();
        let call = Call {
            request_id: "init".into(),
            generation: 42,
        };
        input
            .send(&HostMessage::Initialize {
                protocol_version: 1,
                call: call.clone(),
                entrypoint: "main".into(),
            })
            .await
            .unwrap();
        let mut r = Self {
            child,
            input,
            output,
            call,
            _package: package,
        };
        assert!(matches!(r.receive().await,WorkerMessage::Ready {call,..} if call==r.call));
        r.call.request_id = "run".into();
        r
    }
    async fn receive(&mut self) -> WorkerMessage {
        tokio::time::timeout(Duration::from_secs(15), self.output.receive())
            .await
            .expect("runner deadline")
            .unwrap()
            .expect("worker closed")
    }
    async fn invoke(&mut self, operation: Operation) {
        self.input
            .send(&HostMessage::Invoke {
                call: self.call.clone(),
                operation,
            })
            .await
            .unwrap();
    }
    async fn stop(&mut self) {
        let _ = self.input.send(&HostMessage::Shutdown).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(3), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}
fn character() -> Operation {
    Operation::Character {
        event: CharacterEvent::Activated,
    }
}
#[tokio::test]
async fn luau_awaits_service_and_resumes_ordered_events() {
    let code = br#"return {initialize=function() return {} end,invoke=function(op)
        local visits=ene.service({type="state_read",key="visits"})
        ene.emit({type="character",action={type="speak",text=tostring(visits)}})
        return {visits=visits}
    end}"#;
    let mut r = Runner::start(code, Runtime::Luau).await;
    r.invoke(character()).await;
    let WorkerMessage::Service {
        call,
        service_id,
        request: ServiceRequest::StateRead { .. },
    } = r.receive().await
    else {
        panic!("service request missing")
    };
    r.input
        .send(&HostMessage::ServiceResult {
            call,
            service_id,
            result: Ok(json!(7)),
        })
        .await
        .unwrap();
    assert!(
        matches!(r.receive().await,WorkerMessage::Event {sequence:0,event:Event::Character {action:CharacterAction::Speak {text}},..} if text=="7")
    );
    assert!(matches!(r.receive().await,WorkerMessage::Complete {result,..} if result["visits"]==7));
    r.stop().await;
}
#[tokio::test]
async fn luau_interrupts_infinite_loop() {
    let mut r = Runner::start(
        br#"return {initialize=function() return {} end,invoke=function() while true do end end}"#,
        Runtime::Luau,
    )
    .await;
    r.invoke(character()).await;
    tokio::task::yield_now().await;
    r.input
        .send(&HostMessage::Cancel {
            call: r.call.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(r.receive().await, WorkerMessage::Failed { .. }));
    assert!(
        tokio::time::timeout(Duration::from_secs(3), r.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
fn write(data: &str) -> String {
    let mut code = format!("i64.const {} call $alloc local.set $p\n", data.len());
    for (i, b) in data.bytes().enumerate() {
        code += &format!("local.get $p i64.const {i} i64.add i32.const {b} call $store\n");
    }
    code
}
fn wasm(wait: bool) -> Vec<u8> {
    let caps = write("{}");
    let request = write(
        r#"{"type":"http_open","request":{"method":"GET","url":"https://example.com","headers":{},"body_base64":null}}"#,
    );
    let invoke = if wait {
        format!(
            "{request}local.get $p call $service drop {caps}local.get $p i64.const 2 call $output i32.const 0"
        )
    } else {
        "(loop $forever br $forever) i32.const 0".into()
    };
    wat::parse_str(format!(r#"(module
        (import "extism:host/env" "alloc" (func $alloc (param i64) (result i64)))
        (import "extism:host/env" "store_u8" (func $store (param i64 i32)))
        (import "extism:host/env" "output_set" (func $output (param i64 i64)))
        (import "extism:host/user" "ene_service" (func $service (param i64) (result i64)))
        (func (export "ene_initialize") (result i32) (local $p i64) {caps} local.get $p i64.const 2 call $output i32.const 0)
        (func (export "ene_invoke") (result i32) (local $p i64) {invoke})
    )"#)).unwrap()
}
#[tokio::test]
async fn wasm_cancellation_releases_waiting_host_function() {
    let mut r = Runner::start(&wasm(true), Runtime::Extism).await;
    r.invoke(character()).await;
    assert!(matches!(
        r.receive().await,
        WorkerMessage::Service {
            request: ServiceRequest::HttpOpen { .. },
            ..
        }
    ));
    r.input
        .send(&HostMessage::Cancel {
            call: r.call.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(r.receive().await, WorkerMessage::Failed { .. }));
    assert!(
        tokio::time::timeout(Duration::from_secs(3), r.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
#[tokio::test]
async fn wasm_infinite_loop_stops_with_fuel_or_cancel() {
    let mut r = Runner::start(&wasm(false), Runtime::Extism).await;
    r.invoke(character()).await;
    r.input
        .send(&HostMessage::Cancel {
            call: r.call.clone(),
        })
        .await
        .unwrap();
    assert!(matches!(r.receive().await, WorkerMessage::Failed { .. }));
    let _ = r.child.wait().await;
}
#[tokio::test]
async fn harness_reference_retains_task_state_and_checks_operation_ids() {
    let code = include_bytes!("../../../examples/plugins/reference/harness.luau");
    let mut r = Runner::start(code, Runtime::Luau).await;
    let inputs = [
        HarnessInput::Start {
            instruction: "test".into(),
        },
        HarnessInput::Result {
            operation_id: "model-1".into(),
            result: Value::Null,
        },
        HarnessInput::Result {
            operation_id: "tool-1".into(),
            result: Value::Null,
        },
        HarnessInput::Answer {
            question_id: "question-1".into(),
            answer: "done".into(),
        },
    ];
    for (index, input) in inputs.into_iter().enumerate() {
        r.call.request_id = format!("task-{index}");
        r.invoke(Operation::Harness { input }).await;
        let event = r.receive().await;
        assert!(matches!(
            event,
            WorkerMessage::Event {
                sequence: 0,
                event: Event::Harness { .. },
                ..
            }
        ));
        assert!(matches!(r.receive().await, WorkerMessage::Complete { .. }));
    }
    r.stop().await;
}

#[tokio::test]
async fn output_backpressure_preserves_event_order() {
    let code = br#"return {initialize=function() return {} end,invoke=function()
        for i=1,1000 do ene.emit({type="character",action={type="speak",text=tostring(i)}}) end
        return {}
    end}"#;
    let mut r = Runner::start(code, Runtime::Luau).await;
    r.invoke(character()).await;
    for expected in 0..1000 {
        assert!(
            matches!(r.receive().await,WorkerMessage::Event {sequence,..} if sequence==expected)
        );
    }
    assert!(matches!(r.receive().await, WorkerMessage::Complete { .. }));
    r.stop().await;
}
#[tokio::test]
async fn luau_memory_limit_contains_large_allocations() {
    let mut r=Runner::start(br#"return {initialize=function() return {} end,invoke=function() return string.rep("x",128*1024*1024) end}"#,Runtime::Luau).await;
    r.invoke(character()).await;
    assert!(matches!(r.receive().await, WorkerMessage::Failed { .. }));
    r.stop().await;
}
#[tokio::test]
async fn wrong_service_generation_terminates_the_worker() {
    let mut r=Runner::start(br#"return {initialize=function() return {} end,invoke=function() return ene.service({type="state_read",key="x"}) end}"#,Runtime::Luau).await;
    r.invoke(character()).await;
    let WorkerMessage::Service {
        mut call,
        service_id,
        ..
    } = r.receive().await
    else {
        panic!("missing service");
    };
    call.generation += 1;
    r.input
        .send(&HostMessage::ServiceResult {
            call,
            service_id,
            result: Ok(Value::Null),
        })
        .await
        .unwrap();
    assert!(
        !tokio::time::timeout(Duration::from_secs(3), r.child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}
