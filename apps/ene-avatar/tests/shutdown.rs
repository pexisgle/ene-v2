//! Process shutdown must work on Windows without requiring a GPU or Wayland.
use ene_ipc::renderer::{Renderer, RendererConfig, RendererSession};
use remoc::{Cfg, Connect, ConnectExt};
use std::{process::Stdio, time::Duration};
use tokio::process::{Child, Command};

fn spawn_renderer() -> Child {
    Command::new(env!("CARGO_BIN_EXE_ene-avatar"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn connect(child: &mut Child) -> RendererSession {
    Connect::io::<_, _, RendererSession, RendererSession, remoc::codec::Default>(
        Cfg::default(),
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
    )
    .consume()
    .await
    .unwrap()
}

#[tokio::test]
async fn shutdown_delivers_its_reply_before_exiting_with_state_and_events_still_open() {
    // Repetition exercises the transport/runtime race, not rendering startup.
    for _ in 0..32 {
        let mut child = spawn_renderer();
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut session = connect(&mut child).await;
            session
                .renderer
                .initialize(RendererConfig::default(), vec![])
                .await
                .unwrap();
            session.renderer.shutdown().await.unwrap();
            assert!(child.wait().await.unwrap().success());
            drop((session.messages, session.states));
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn dropping_the_client_exits_even_with_other_session_handles_open() {
    let mut child = spawn_renderer();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut session = connect(&mut child).await;
        session
            .renderer
            .initialize(RendererConfig::default(), vec![])
            .await
            .unwrap();
        drop(session.renderer);
        assert!(child.wait().await.unwrap().success());
        drop((session.messages, session.states));
    })
    .await
    .unwrap();
}
