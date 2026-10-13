#![cfg(unix)]

use ene_ipc::renderer::{
    AvatarConfig, AvatarId, AvatarInstance, AvatarPlacement, Renderer, RendererConfig,
    RendererError, RendererSession,
};
use remoc::{Cfg, Connect, ConnectExt};
use std::{process::Stdio, time::Duration};
use tokio::process::{Child, Command};

const TIMEOUT: Duration = Duration::from_secs(5);

fn spawn_renderer() -> Child {
    Command::new(env!("CARGO_BIN_EXE_ene-avatar"))
        .env(
            "WAYLAND_DISPLAY",
            format!("/tmp/ene-ipc-no-wayland-{}", std::process::id()),
        )
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

fn avatar(id: u64, x: i32) -> AvatarInstance {
    AvatarInstance {
        id: AvatarId(id),
        config: AvatarConfig::default(),
        placement: AvatarPlacement {
            x,
            ..Default::default()
        },
    }
}

fn rejection(result: Result<(), RendererError>, expected: &str) {
    assert!(matches!(result, Err(RendererError::Rejected(message)) if message.contains(expected)));
}

#[tokio::test]
async fn lifecycle_returns_operation_results_and_shutdown_exits_with_session_open() {
    let mut child = spawn_renderer();
    tokio::time::timeout(TIMEOUT, async {
        let mut client = connect(&mut child).await;
        let first = avatar(1, 50);
        let second = avatar(2, 60);
        client
            .renderer
            .initialize(RendererConfig::default(), vec![first.clone(), second])
            .await
            .unwrap();
        // Failed native windows retain their identities and state until removal.
        assert_eq!(
            client
                .states
                .wait_for(|state| {
                    state.avatars.len() == 2
                        && state
                            .avatars
                            .values()
                            .all(|avatar| avatar.error.is_some() && !avatar.loading)
                })
                .await
                .unwrap()
                .avatars[&first.id]
                .placement
                .x,
            50
        );
        rejection(
            client.renderer.add_avatar(first.clone()).await,
            "already exists",
        );
        client.renderer.remove_avatar(first.id).await.unwrap();
        assert!(
            client
                .states
                .wait_for(|state| !state.avatars.contains_key(&first.id))
                .await
                .unwrap()
                .avatars
                .contains_key(&AvatarId(2))
        );
        client.renderer.add_avatar(avatar(3, 80)).await.unwrap();
        client
            .states
            .wait_for(|state| state.avatars.len() == 2)
            .await
            .unwrap();
        // The consuming call must flush its reply even with other session handles open.
        client.renderer.shutdown().await.unwrap();
        assert!(child.wait().await.unwrap().success());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn queued_calls_apply_in_order_without_partial_initialization() {
    let mut child = spawn_renderer();
    tokio::time::timeout(TIMEOUT, async {
        let mut client = connect(&mut child).await;
        let first = avatar(1, 50);
        rejection(
            client
                .renderer
                .initialize(
                    RendererConfig::default(),
                    vec![first.clone(), first.clone()],
                )
                .await,
            "Duplicate",
        );
        assert_eq!(*client.states.borrow().unwrap(), Default::default());
        let mut invalid = avatar(2, 80);
        invalid.config.size_percent = 49;
        rejection(
            client
                .renderer
                .initialize(RendererConfig::default(), vec![first.clone(), invalid])
                .await,
            "50%",
        );
        assert_eq!(*client.states.borrow().unwrap(), Default::default());

        // _call queues calls without waiting for their results. The generated server
        // processes mutable operations serially and associates each reply itself.
        let initialized = client
            .renderer
            .initialize_call(RendererConfig::default(), vec![])
            .await;
        let added = client.renderer.add_avatar_call(first.clone()).await;
        let duplicate = client.renderer.add_avatar_call(first).await;
        let removed = client.renderer.remove_avatar_call(AvatarId(1)).await;
        let added_second = client.renderer.add_avatar_call(avatar(2, 80)).await;
        let unknown = client.renderer.recenter_call(AvatarId(99)).await;
        initialized.await.unwrap();
        added.await.unwrap();
        rejection(duplicate.await, "already exists");
        removed.await.unwrap();
        added_second.await.unwrap();
        rejection(unknown.await, "Unknown");
        assert_eq!(
            client
                .states
                .wait_for(|state| state
                    .avatars
                    .get(&AvatarId(2))
                    .is_some_and(|avatar| avatar.placement.x == 80))
                .await
                .unwrap()
                .avatars
                .len(),
            1
        );
        client.renderer.shutdown().await.unwrap();
        assert!(child.wait().await.unwrap().success());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invalid_operations_return_rejections_and_leave_the_session_usable() {
    let mut child = spawn_renderer();
    tokio::time::timeout(TIMEOUT, async {
        let mut client = connect(&mut child).await;
        rejection(
            client.renderer.add_avatar(avatar(1, 50)).await,
            "initialized first",
        );
        rejection(
            client
                .renderer
                .set_render_config(RendererConfig { fps: 60 })
                .await,
            "initialized first",
        );
        rejection(
            client.renderer.remove_avatar(AvatarId(1)).await,
            "initialized first",
        );
        client
            .renderer
            .initialize(RendererConfig::default(), vec![avatar(1, 50)])
            .await
            .unwrap();
        rejection(
            client
                .renderer
                .initialize(RendererConfig::default(), vec![])
                .await,
            "already initialized",
        );
        rejection(
            client
                .renderer
                .set_avatar_config(
                    AvatarId(1),
                    AvatarConfig {
                        size_percent: 201,
                        ..Default::default()
                    },
                )
                .await,
            "200%",
        );
        rejection(
            client
                .renderer
                .set_avatar_config(AvatarId(99), AvatarConfig::default())
                .await,
            "Unknown",
        );
        rejection(client.renderer.remove_avatar(AvatarId(99)).await, "Unknown");
        client
            .states
            .wait_for(|state| state.avatars[&AvatarId(1)].error.is_some())
            .await
            .unwrap();
        rejection(
            client.renderer.recenter(AvatarId(1)).await,
            "window stopped",
        );
        client
            .renderer
            .set_render_config(RendererConfig { fps: 60 })
            .await
            .unwrap();
        assert_eq!(
            client
                .states
                .wait_for(|state| state.config.as_ref().is_some_and(|config| config.fps == 60))
                .await
                .unwrap()
                .avatars
                .len(),
            1
        );
        // Per-operation rejections are RPC results; only the asynchronous broadcast
        // failure from set_render_config is an event-channel error.
        let message = client.messages.recv().await.unwrap().unwrap();
        assert!(matches!(
            message,
            ene_ipc::renderer::RendererMessage::Error {
                avatar_id: Some(AvatarId(1)),
                ..
            }
        ));
        client.renderer.shutdown().await.unwrap();
        assert!(child.wait().await.unwrap().success());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn dropping_the_session_exits_the_renderer() {
    let mut child = spawn_renderer();
    tokio::time::timeout(TIMEOUT, async {
        let mut client = connect(&mut child).await;
        client
            .renderer
            .initialize(RendererConfig::default(), vec![avatar(1, 50)])
            .await
            .unwrap();
        drop(client);
        assert!(child.wait().await.unwrap().success());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn unread_events_do_not_block_operations_or_shutdown() {
    let mut child = spawn_renderer();
    tokio::time::timeout(TIMEOUT, async {
        let mut client = connect(&mut child).await;
        client
            .renderer
            .initialize(RendererConfig::default(), vec![avatar(1, 50)])
            .await
            .unwrap();
        client
            .states
            .wait_for(|state| state.avatars[&AvatarId(1)].error.is_some())
            .await
            .unwrap();
        // Every FPS broadcast to the failed window produces an event. Deliberately
        // leave messages unread beyond the remote channel buffer capacity.
        for fps in 1..=128 {
            client
                .renderer
                .set_render_config(RendererConfig { fps })
                .await
                .unwrap();
        }
        client
            .states
            .wait_for(|state| {
                state
                    .config
                    .as_ref()
                    .is_some_and(|config| config.fps == 128)
            })
            .await
            .unwrap();
        client.renderer.shutdown().await.unwrap();
        assert!(child.wait().await.unwrap().success());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn terminated_renderer_returns_a_call_error() {
    let mut child = spawn_renderer();
    tokio::time::timeout(TIMEOUT, async {
        let mut client = connect(&mut child).await;
        child.kill().await.unwrap();
        assert!(matches!(
            client.renderer.recenter(AvatarId(1)).await,
            Err(RendererError::Call(_))
        ));
    })
    .await
    .unwrap();
}
