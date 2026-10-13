// WGPU's nested resource types need a deeper trait check on nightly.
#![recursion_limit = "256"]

#[cfg(all(unix, feature = "dev-dynamic-linking"))]
#[expect(
    unused_imports,
    clippy::single_component_path_imports,
    reason = "Importing the dylib keeps Bevy linked dynamically in Unix development builds."
)]
use bevy_dylib;
#[cfg(all(unix, feature = "dev-dynamic-linking"))]
use bevy_internal as _;

mod application;
mod ipc;
mod render;
#[cfg_attr(windows, path = "native_windows.rs")]
mod wayland;

use ene_ipc::renderer::{Renderer, RendererConfig, RendererServer, RendererSession};
use futures_util::StreamExt;
use remoc::rtc::Server;
use remoc::{Cfg, Connect, ConnectExt, rch};

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    use clap::Parser;
    match run(application::Args::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ene-avatar: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: application::Args) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Each native window owns its Bevy renderer on its worker thread.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let standalone = args.standalone;
        let (messages, events) = rch::mpsc::channel();
        let (event_sender, event_receiver) = futures_channel::mpsc::unbounded();
        tokio::spawn(
            event_receiver
                .map(Ok)
                .forward(rch::mpsc::SenderSink::new(messages)),
        );
        let (states, updates) = rch::watch::channel(ene_ipc::renderer::RendererState::default());
        let output = ipc::Output {
            messages: (!standalone).then_some(event_sender),
            states: std::sync::Arc::new(states),
        };
        let mut service = ipc::RendererService::new(output);
        if let Some(avatar) = args.initial_avatar() {
            service
                .initialize(RendererConfig::default(), vec![avatar])
                .await?;
        }
        let (server, renderer) = RendererServer::new(service);
        let client = RendererSession {
            renderer,
            messages: events,
            states: updates,
        };
        let _local = if standalone {
            Some(client)
        } else {
            #[cfg(unix)]
            let (input, output) = {
                use std::os::fd::AsFd;
                use tokio::net::unix::pipe;
                // IPC owns these pipe ends. Unlike Tokio Stdin, they have no
                // uncancellable blocking read that can delay process shutdown.
                let input =
                    pipe::Receiver::from_owned_fd(std::io::stdin().as_fd().try_clone_to_owned()?)?;
                let output =
                    pipe::Sender::from_owned_fd(std::io::stdout().as_fd().try_clone_to_owned()?)?;
                (input, output)
            };
            #[cfg(windows)]
            let (input, output) = (tokio::io::stdin(), tokio::io::stdout());
            Connect::io::<_, _, RendererSession, RendererSession, remoc::codec::Default>(
                Cfg::default(),
                input,
                output,
            )
            .provide(client)
            .await?;
            None
        };
        let (_service, result) = server.serve().await;
        result.map_err(|error| error.to_string())?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    result
}
