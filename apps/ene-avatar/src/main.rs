// WGPU's nested resource types need a deeper trait check on nightly.
#![recursion_limit = "256"]

mod application;
mod ipc;
mod render;
#[cfg_attr(windows, path = "native_windows.rs")]
mod wayland;

use ene_ipc::{
    remoc::{self, Cfg, Connect, ConnectExt, rch},
    renderer::RendererClient,
};

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    use clap::Parser;
    match run(application::Args::parse()) {
        Ok(exit) if !exit.is_error() => std::process::ExitCode::SUCCESS,
        Ok(_) => std::process::ExitCode::FAILURE,
        Err(error) => {
            eprintln!("ene-avatar: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(
    args: application::Args,
) -> Result<bevy::app::AppExit, Box<dyn std::error::Error + Send + Sync>> {
    // Bevy runs on the main thread; the runtime drives only asynchronous IPC.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let (receiver, output, local) = runtime.block_on(async {
        let (requests, receiver) = rch::mpsc::channel();
        let (messages, events) = rch::mpsc::channel();
        let (states, updates) = rch::watch::channel(ene_ipc::renderer::RendererState::default());
        let client = RendererClient {
            requests,
            messages: events,
            states: updates,
        };
        let output = ipc::Output {
            messages: (!args.standalone).then_some(messages),
            states: std::sync::Arc::new(states),
        };
        let local = if args.standalone {
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
            Connect::io::<_, _, RendererClient, RendererClient, remoc::codec::Default>(
                Cfg::default(),
                input,
                output,
            )
            .provide(client)
            .await?;
            None
        };
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((receiver, output, local))
    })?;
    let exit = application::run(args, receiver, output);
    drop(local);
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    Ok(exit)
}
