//! Speech synthesis and recognition engine resolution (`HttpEngine` and plugin adapters).

use super::{HttpEngine, SetupError, SttEngine, TtsEngine, parse_binding, plugin};
use crate::host::{
    db::Store,
    mind::Companion,
    model::{self, Credentials},
    net,
    providers::Providers,
    settings::{Connection, ConnectionKind},
};
use bevy_ecs::prelude::*;
use ene_rpc as w;
use std::sync::Arc;

/// Engines a test injects to stand in for the plugin path.
#[derive(Resource, Default)]
pub(crate) struct Engines {
    pub(crate) tts: Option<Arc<dyn TtsEngine>>,
    pub(crate) stt: Option<Arc<dyn SttEngine>>,
}

fn load_connection_secret(
    connection: &Connection,
    credentials: Option<&Credentials>,
    providers: Option<&Providers>,
) -> std::result::Result<Option<String>, SetupError> {
    match credentials {
        Some(creds) => model::connection_secret(
            connection,
            Some(creds),
            providers.unwrap_or(&Providers::none()),
        )
        .map_err(|error| SetupError::Broken(error.to_string())),
        None => Ok(None),
    }
}

fn resolve_base(connection: &Connection, providers: Option<&Providers>) -> String {
    let fallback = Providers::none();
    let providers = providers.unwrap_or(&fallback);
    crate::host::settings::endpoint_of(connection, providers).unwrap_or_else(|| {
        connection
            .endpoint()
            .endpoint
            .trim()
            .trim_end_matches('/')
            .to_owned()
    })
}

fn http_engine(
    world: &World,
    connection: Connection,
    kind: ConnectionKind,
    binding: Option<&w::VoiceBinding>,
) -> std::result::Result<HttpEngine, SetupError> {
    let credentials = world.get_resource::<Credentials>();
    let providers = world.get_resource::<Providers>();
    let resolved_base = resolve_base(&connection, providers);
    let secret = load_connection_secret(&connection, credentials, providers)?;
    let client = net::client().map_err(|error| SetupError::Broken(error.to_string()))?;
    let engine = HttpEngine::new(connection, resolved_base, secret, kind, client)?;
    Ok(match binding {
        Some(binding) => engine.with_binding(binding),
        None => engine,
    })
}

pub(crate) fn tts_source(world: &World) -> std::result::Result<Arc<dyn TtsEngine>, SetupError> {
    companion_tts_source(world, None)
}

pub(crate) fn companion_tts_source(
    world: &World,
    companion: Option<&Companion>,
) -> std::result::Result<Arc<dyn TtsEngine>, SetupError> {
    if let Some(engine) = world
        .get_resource::<Engines>()
        .and_then(|test| test.tts.as_ref())
    {
        return Ok(Arc::clone(engine));
    }
    let binding = companion.map(parse_binding).unwrap_or_default();
    let connection = match binding
        .connection_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        Some(connection_id) => voice_connection_by_id(world, ConnectionKind::Tts, connection_id)?,
        None => voice_connection(world, ConnectionKind::Tts)?,
    };
    if let Some(engine) = plugin_engine(world, &connection, ConnectionKind::Tts)? {
        return Ok(Arc::new(engine));
    }
    if matches!(
        connection.endpoint().adapter.as_str(),
        "voicevox" | "openai" | "sakura"
    ) {
        let engine = http_engine(world, connection, ConnectionKind::Tts, Some(&binding))?;
        return Ok(Arc::new(engine));
    }
    Err(SetupError::Unsupported(format!(
        "the TTS adapter `{}` is not served by an installed plugin",
        connection.endpoint().adapter
    )))
}

pub(crate) fn stt_source(world: &World) -> std::result::Result<Arc<dyn SttEngine>, SetupError> {
    if let Some(engine) = world
        .get_resource::<Engines>()
        .and_then(|test| test.stt.as_ref())
    {
        return Ok(Arc::clone(engine));
    }
    let connection = voice_connection(world, ConnectionKind::Stt)?;
    if let Some(engine) = plugin_engine(world, &connection, ConnectionKind::Stt)? {
        return Ok(Arc::new(engine));
    }
    if matches!(connection.endpoint().adapter.as_str(), "openai" | "sakura") {
        let engine = http_engine(world, connection, ConnectionKind::Stt, None)?;
        return Ok(Arc::new(engine));
    }
    Err(SetupError::Unsupported(format!(
        "the STT adapter `{}` is not served by an installed plugin",
        connection.endpoint().adapter
    )))
}

fn plugin_engine(
    world: &World,
    connection: &Connection,
    kind: ConnectionKind,
) -> std::result::Result<Option<plugin::PluginSpeech>, SetupError> {
    let Some(providers) = world.get_resource::<Providers>() else {
        return Ok(None);
    };
    plugin::PluginSpeech::new(
        providers,
        connection,
        world.get_resource::<Credentials>(),
        kind,
    )
}

fn voice_connection_by_id(
    world: &World,
    kind: ConnectionKind,
    id: &str,
) -> std::result::Result<Connection, SetupError> {
    let Some(store) = world.get_resource::<Store>() else {
        return Err(SetupError::Broken("the host state is not available".into()));
    };
    let settings = store
        .settings_repo()
        .settings()
        .map_err(|error| SetupError::Broken(error.to_string()))?;
    ene_voice::select_connection_by_id(&settings.connections, kind, id)
}

pub(super) fn voice_connection(
    world: &World,
    kind: ConnectionKind,
) -> std::result::Result<Connection, SetupError> {
    let Some(store) = world.get_resource::<Store>() else {
        return Err(SetupError::Broken("the host state is not available".into()));
    };
    let settings = store
        .settings_repo()
        .settings()
        .map_err(|error| SetupError::Broken(error.to_string()))?;
    ene_voice::select_connection(&settings.connections, kind)
}
