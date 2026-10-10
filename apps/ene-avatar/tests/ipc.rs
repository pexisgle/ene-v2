use ene_ipc::{
    remoc::{self, Cfg, Connect, ConnectExt},
    renderer::{
        AvatarConfig, AvatarId, AvatarInstance, AvatarPlacement, RendererClient, RendererConfig,
        RendererMessage, RendererRequest,
    },
};
use std::{process::Stdio, time::Duration};
use tokio::process::{Child, Command};

fn spawn_renderer() -> Child {
    Command::new(env!("CARGO_BIN_EXE_ene-avatar"))
        .env(
            "WAYLAND_DISPLAY",
            format!("/tmp/ene-ipc-no-wayland-{}", AvatarId::new()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

async fn connect(child: &mut Child) -> RendererClient {
    Connect::io::<_, _, _, _, remoc::codec::Default>(
        Cfg::default(),
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
    )
    .consume()
    .await
    .unwrap()
}

async fn send(client: &RendererClient, request: RendererRequest) {
    client.requests.send(request).await.unwrap().await.unwrap();
}

#[tokio::test]
async fn ecs_lifecycle_keeps_other_states_and_shutdown_exits_with_client_open() {
    let mut child = spawn_renderer();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut client = connect(&mut child).await;
        let first = AvatarInstance {
            id: AvatarId::new(), config: AvatarConfig::default(),
            placement: AvatarPlacement { x: 50, ..Default::default() },
        };
        let second = AvatarInstance { id: AvatarId::new(), ..first.clone() };
        send(&client, RendererRequest::Initialize {
            config: RendererConfig::default(), avatars: vec![first.clone(), second.clone()],
        }).await;
        // Wayland fails intentionally. Failed windows retain their identities and
        // states until removal, and removing one must leave the other untouched.
        assert_eq!(client.states.wait_for(|state| {
            state.avatars.len() == 2 && state.avatars.values().all(|avatar| avatar.error.is_some() && !avatar.loading)
        }).await.unwrap().avatars[&first.id].placement.x, 50);
        send(&client, RendererRequest::AddAvatar { avatar: first.clone() }).await;
        assert!(matches!(client.messages.recv().await.unwrap().unwrap(), RendererMessage::Error { avatar_id: Some(id), .. } if id == first.id));
        send(&client, RendererRequest::RemoveAvatar { avatar_id: first.id }).await;
        assert_eq!(client.messages.recv().await.unwrap().unwrap(), RendererMessage::AvatarRemoved { avatar_id: first.id });
        assert!(client.states.wait_for(|state| !state.avatars.contains_key(&first.id)).await.unwrap().avatars.contains_key(&second.id));
        send(&client, RendererRequest::AddAvatar { avatar: first }).await;
        client.states.wait_for(|state| state.avatars.len() == 2).await.unwrap();
        // A peer must be able to request shutdown while still holding its handles.
        // Keeping them open detects uncancellable stdin reads in the child runtime.
        send(&client, RendererRequest::Shutdown).await;
        assert!(child.wait().await.unwrap().success());
        drop(client);
    }).await.unwrap();
}

#[tokio::test]
async fn queued_lifecycle_requests_apply_in_order_without_partial_initialization() {
    let mut child = spawn_renderer();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut client = connect(&mut child).await;
        let mut avatar = AvatarInstance {
            id: AvatarId::new(), config: AvatarConfig::default(),
            placement: AvatarPlacement { x: 50, ..Default::default() },
        };
        send(&client, RendererRequest::Initialize {
            config: RendererConfig::default(), avatars: vec![avatar.clone(), avatar.clone()],
        }).await;
        assert!(matches!(client.messages.recv().await.unwrap().unwrap(), RendererMessage::Error { avatar_id: None, .. }));
        assert_eq!(*client.states.borrow().unwrap(), Default::default());

        let id = avatar.id;
        let unknown = AvatarId::new();
        // Queue the entire sequence before reading notifications. Commands from
        // each registered-system invocation must be applied before the next one.
        send(&client, RendererRequest::Initialize { config: RendererConfig::default(), avatars: vec![] }).await;
        send(&client, RendererRequest::AddAvatar { avatar: avatar.clone() }).await;
        send(&client, RendererRequest::AddAvatar { avatar: avatar.clone() }).await;
        send(&client, RendererRequest::RemoveAvatar { avatar_id: id }).await;
        avatar.placement.x = 80;
        send(&client, RendererRequest::AddAvatar { avatar }).await;
        send(&client, RendererRequest::Recenter { avatar_id: unknown }).await;
        assert!(matches!(client.messages.recv().await.unwrap().unwrap(), RendererMessage::Error { avatar_id: Some(avatar_id), .. } if avatar_id == id));
        assert_eq!(client.messages.recv().await.unwrap().unwrap(), RendererMessage::AvatarRemoved { avatar_id: id });
        assert!(matches!(client.messages.recv().await.unwrap().unwrap(), RendererMessage::Error { avatar_id: Some(avatar_id), .. } if avatar_id == unknown));
        assert_eq!(client.states.wait_for(|state| state.avatars.get(&id).is_some_and(|avatar| avatar.placement.x == 80)).await.unwrap().avatars.len(), 1);
        send(&client, RendererRequest::Shutdown).await;
        assert!(child.wait().await.unwrap().success());
    }).await.unwrap();
}
