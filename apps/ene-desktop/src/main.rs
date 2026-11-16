//! Starts the desktop client against a running `ene-host`.
//!
//! The client takes no arguments: like the host, it is configured by the
//! discovery file it is pointed at and by its environment.

use ene_desktop::{Client, Discovery, Host, Options};
use std::{path::PathBuf, time::Duration};
/// How often the client samples the OS for perception.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(20);

/// How often the client sends the playback amplitude to the avatar.
const LIP_SYNC_INTERVAL: Duration = Duration::from_millis(33);

/// How often captured microphone audio is sent to the host.
const CAPTURE_INTERVAL: Duration = Duration::from_millis(100);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let discovery = Discovery::read(&discovery_path()?)?;
    let host = Host::connect(&discovery)?;
    let mut client = Client::start(host, options()).await?;
    let companion = client.companion().await?;
    client.activate(&companion).await?;
    tracing::info!(
        companion = %companion,
        revision = client.activation().revision,
        granted = ?client.granted(),
        "the client is the active device"
    );
    if client.granted().is_empty() {
        tracing::warn!(
            "the host granted no capability; set ENE_HOST_ALLOWED_CAPABILITIES on the host \
             to let the model call this client's tools"
        );
    }

    run(&mut client).await;
    client.shutdown();
    Ok(())
}

/// Serves the device stream and the audio stream until the process is asked to
/// stop.
async fn run(client: &mut Client) {
    let mut stream = match client.device_stream().await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::error!("the device stream did not open: {error}");
            return;
        }
    };
    // A host with no TTS connection answers `unimplemented`; that is a client
    // without audio, not a failure to start.
    let mut audio = match client.audio_stream().await {
        Ok(stream) => Some(stream),
        Err(error) => {
            tracing::info!("no audio output stream: {error}");
            None
        }
    };
    // A host with no STT connection answers `unimplemented`; that is a client
    // without a microphone, not a failure to start.
    let mut microphone = match client.microphone_stream().await {
        Ok(stream) => {
            client.open_microphone();
            Some(stream)
        }
        Err(error) => {
            tracing::info!("no microphone stream: {error}");
            None
        }
    };
    let mut sample = tokio::time::interval(SAMPLE_INTERVAL);
    let mut lip_sync = tokio::time::interval(LIP_SYNC_INTERVAL);
    let mut capture = tokio::time::interval(CAPTURE_INTERVAL);
    loop {
        tokio::select! {
            frame = stream.message() => match frame {
                Ok(Some(message)) => {
                    let event = message.to_owned_message();
                    if let Some(event) = event.event {
                        handle(client, event).await;
                    }
                }
                Ok(None) => {
                    tracing::info!("the host closed the device stream");
                    return;
                }
                Err(error) => {
                    tracing::error!("the device stream failed: {error}");
                    return;
                }
            },
            frame = audio_message(&mut audio) => match frame {
                Some(Ok(event)) => client.play(event),
                Some(Err(error)) => {
                    // A host with no TTS connection reports `unimplemented` on
                    // the first frame. That is a client without audio, not a
                    // stream that broke.
                    if error.code == connectrpc::ErrorCode::Unimplemented {
                        tracing::info!("the host has no audio output: {error}");
                    } else {
                        tracing::error!("the audio stream failed: {error}");
                    }
                    audio = None;
                }
                None => audio = None,
            },
            Some(chunk) = capture_tick(&mut capture, client) => {
                if let Some(stream) = microphone.as_mut()
                    && let Err(error) = stream.send(chunk).await
                {
                    // The host closes the microphone stream when it has no use
                    // for one; that is a client without a microphone, not a
                    // reason to stop serving the device stream.
                    tracing::info!("the microphone stream closed: {error}");
                    microphone = None;
                }
            }
            _ = lip_sync.tick() => {
                client.lip_sync();
            }
            _ = sample.tick() => {
                if let Err(error) = client.sample().await {
                    tracing::warn!("perception was not reported: {error}");
                }
                for notification in client.drain_notifications() {
                    tracing::debug!(?notification, "renderer");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("stopping");
                return;
            }
        }
    }
}

/// One captured block, when the microphone is due and has audio.
async fn capture_tick(
    interval: &mut tokio::time::Interval,
    client: &Client,
) -> Option<ene_rpc::AudioInputChunk> {
    interval.tick().await;
    client.capture()
}

/// One frame from the audio stream.
///
/// An absent stream parks this branch forever instead of returning at once.
/// Returning `None` immediately would make the `select!` branch permanently
/// ready and spin a core, which is exactly what a host without TTS caused.
async fn audio_message(
    audio: &mut Option<
        connectrpc::client::ServerStream<
            <connectrpc::client::HttpClient as connectrpc::client::ClientTransport>::ResponseBody,
            ene_rpc::__buffa::view::AudioOutputEventView<'static>,
        >,
    >,
) -> Option<Result<ene_rpc::audio_output_event::Event, connectrpc::ConnectError>> {
    let Some(stream) = audio.as_mut() else {
        return std::future::pending().await;
    };
    match stream.message().await {
        Ok(Some(message)) => {
            let event = message.to_owned_message();
            Some(Ok(event).and_then(|event| {
                event
                    .event
                    .ok_or_else(|| connectrpc::ConnectError::internal("an empty audio event"))
            }))
        }
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

/// Applies one frame from the host.
async fn handle(client: &mut Client, event: ene_rpc::device_event::Event) {
    match event {
        ene_rpc::device_event::Event::Ready(ready) => {
            tracing::info!(subscription = %ready.subscription_id, "device stream ready");
        }
        ene_rpc::device_event::Event::Operation(operation) => {
            let result = client.apply(&operation);
            if let Err(error) = client.complete(result).await {
                tracing::warn!("the operation result was not reported: {error}");
            }
        }
    }
}

/// The discovery file: `ENE_HOST_CONNECTION`, or the host's default runtime
/// directory.
fn discovery_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = std::env::var_os("ENE_HOST_CONNECTION") {
        return Ok(PathBuf::from(path));
    }
    let dirs = directories::ProjectDirs::from("", "", "ene")
        .ok_or("no runtime directory is configured; set ENE_HOST_CONNECTION")?;
    let runtime = dirs
        .runtime_dir()
        .map_or_else(|| dirs.cache_dir().join("runtime"), PathBuf::from);
    Ok(runtime.join("connection.json"))
}

/// The client's identity and renderer, from the environment.
fn options() -> Options {
    let defaults = Options::default();
    Options {
        id: std::env::var("ENE_DESKTOP_CLIENT_ID").unwrap_or(defaults.id),
        name: std::env::var("ENE_DESKTOP_CLIENT_NAME").unwrap_or(defaults.name),
        platform: defaults.platform,
        renderer: std::env::var_os("ENE_DESKTOP_RENDERER").map(PathBuf::from),
        model: std::env::var_os("ENE_DESKTOP_MODEL")
            .map(PathBuf::from)
            .or(defaults.model),
    }
}
