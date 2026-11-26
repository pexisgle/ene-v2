//! Bounded HTTP probes for speech and embedding connections.

use crate::host::{error::Failure, model, net, providers::Providers, settings};
use ene_store::util::now;
use reqwest::{Client, IntoUrl, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, de::DeserializeOwned};
use std::time::Duration;
use url::{Host, Url};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const METADATA_LIMIT: usize = 2 * 1024 * 1024;

pub(super) fn healthy(checked_at: String, resources: Vec<String>) -> settings::Check {
    settings::Check {
        healthy: true,
        checked_at,
        resources,
        ..Default::default()
    }
}

pub(super) fn failed(checked_at: String, message: impl Into<String>) -> settings::Check {
    settings::Check {
        checked_at,
        failure: Some(Failure::new("connection_check", message)),
        ..Default::default()
    }
}

#[derive(Debug, thiserror::Error)]
enum MetadataError {
    #[error(transparent)]
    Read(#[from] net::NetError),
    #[error("invalid probe metadata: {0}")]
    Decode(#[from] serde_json::Error),
}

async fn metadata<T: DeserializeOwned>(
    response: Response,
    label: &str,
) -> Result<T, MetadataError> {
    let bytes = net::read_response_body(response, METADATA_LIMIT, label).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Deserialize)]
struct Speaker {
    name: Option<String>,
    #[serde(default)]
    styles: Vec<SpeakerStyle>,
}

#[derive(Deserialize)]
struct SpeakerStyle {
    id: u64,
    name: Option<String>,
}

impl Speaker {
    fn resources(self) -> impl Iterator<Item = String> {
        self.styles.into_iter().map(move |style| {
            let speaker = self.name.as_deref().unwrap_or_default().trim();
            let name = style.name.as_deref().unwrap_or_default().trim();
            let id = style.id;
            match (speaker.is_empty(), name.is_empty()) {
                (false, false) => format!("{id}:{speaker}（{name}）"),
                (false, true) => format!("{id}:{speaker}"),
                (true, false) => format!("{id}:{name}"),
                (true, true) => id.to_string(),
            }
        })
    }
}

pub(super) async fn probe_voicevox_tts(
    connection: &settings::Connection,
    voicevox: &crate::host::voice::voicevox::VoicevoxManager,
) -> settings::Check {
    let checked_at = now();
    let endpoint = connection.endpoint();
    let base = endpoint.endpoint.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        "http://127.0.0.1:50021"
    } else {
        base
    };
    let url = format!("{base}/speakers");
    let client = match net::client() {
        Ok(client) => client,
        Err(error) => return failed(checked_at, error.to_string()),
    };
    let request = || client.get(&url).timeout(Duration::from_secs(5)).send();
    let response = match request().await {
        Ok(response) => Ok(response),
        Err(_) => match voicevox.ensure_running(base, &endpoint.options).await {
            Ok(()) => request().await,
            Err(error) => return failed(checked_at, error.to_string()),
        },
    };
    match response {
        Ok(response) if response.status().is_success() => {
            match metadata::<Vec<Speaker>>(response, "VOICEVOX speakers").await {
                Ok(speakers) => healthy(
                    checked_at,
                    speakers.into_iter().flat_map(Speaker::resources).collect(),
                ),
                Err(error) => failed(checked_at, error.to_string()),
            }
        }
        Ok(response) => failed(checked_at, format!("HTTP {}", response.status().as_u16())),
        Err(error) => failed(checked_at, error.without_url().to_string()),
    }
}

fn get(client: &Client, url: impl IntoUrl, secret: Option<&str>) -> RequestBuilder {
    let request = client.get(url).timeout(PROBE_TIMEOUT);
    match secret {
        Some(key) => request.bearer_auth(key),
        None => request,
    }
}

fn auth_failed(status: StatusCode) -> bool {
    matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
}

fn auth_message(status: StatusCode) -> String {
    format!(
        "認証エラー (HTTP {})。APIキー（アカウントトークン）を確認してください。",
        status.as_u16()
    )
}

#[derive(Deserialize)]
struct Models {
    data: Vec<ListedModel>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ListedModel {
    Identified { id: String },
    Unknown(serde::de::IgnoredAny),
}

// The optional catalogue is separate from whether the service answers the probe.
async fn read_listed_model_ids(response: Response) -> Vec<String> {
    match metadata::<Models>(response, "model list").await {
        Ok(models) => models
            .data
            .into_iter()
            .filter_map(|model| match model {
                ListedModel::Identified { id } => Some(id),
                ListedModel::Unknown(_) => None,
            })
            .collect(),
        Err(error) => {
            tracing::debug!(%error, "probe did not receive a model list");
            Vec::new()
        }
    }
}

pub(super) async fn probe_openai_speech(
    connection: &settings::Connection,
    credentials: &model::Credentials,
    providers: &Providers,
) -> settings::Check {
    let checked_at = now();
    let base = settings::endpoint_of(connection, providers).unwrap_or_else(|| {
        connection
            .endpoint()
            .endpoint
            .trim()
            .trim_end_matches('/')
            .to_owned()
    });
    let Ok(parsed) = Url::parse(&base) else {
        return failed(checked_at, "接続先URLが正しくありません");
    };
    let sakura = model::is_sakura_connection(&connection.endpoint().adapter, &base);
    let secret = match model::connection_secret(connection, Some(credentials), providers) {
        Ok(secret) => secret,
        Err(error) => return failed(checked_at, error.to_string()),
    };
    let client = match net::client() {
        Ok(client) => client,
        Err(error) => return failed(checked_at, error.to_string()),
    };
    let models_url = if sakura && base.ends_with("/tts/v1") {
        format!("{}/v1/models", base.trim_end_matches("/tts/v1"))
    } else {
        format!("{base}/models")
    };
    match get(&client, &models_url, secret.as_deref()).send().await {
        Ok(response) if response.status().is_success() => {
            let listed = read_listed_model_ids(response).await;
            healthy(
                checked_at,
                collect_speech_resources(connection.kind, sakura, listed),
            )
        }
        Ok(response) if auth_failed(response.status()) => {
            failed(checked_at, auth_message(response.status()))
        }
        Ok(response) => {
            if loopback_speech_health(&client, &parsed, secret.as_deref()).await {
                healthy(checked_at, Vec::new())
            } else {
                failed(checked_at, format!("HTTP {}", response.status().as_u16()))
            }
        }
        Err(error) => failed(checked_at, error.without_url().to_string()),
    }
}

fn collect_speech_resources(
    kind: settings::ConnectionKind,
    sakura: bool,
    listed: Vec<String>,
) -> Vec<String> {
    if sakura && kind == settings::ConnectionKind::Tts {
        let mut out: Vec<String> = model::SAKURA_TTS_VOICES
            .iter()
            .map(|entry| (*entry).to_owned())
            .collect();
        for id in listed {
            if model::is_sakura_tts_model_id(&id)
                && !out.iter().any(|entry| {
                    entry
                        .split('/')
                        .next()
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&id))
                })
            {
                out.push(format!("{id}/normal:VOICEVOX:{id}"));
            }
        }
        out
    } else if sakura && kind == settings::ConnectionKind::Stt {
        let mut out: Vec<String> = listed
            .into_iter()
            .filter(|id| {
                let lower = id.to_ascii_lowercase();
                lower.contains("whisper") || lower.contains("transcribe")
            })
            .collect();
        for id in model::SAKURA_STT_MODELS {
            if !out.iter().any(|existing| existing == id) {
                out.push((*id).to_owned());
            }
        }
        out
    } else {
        listed
    }
}

async fn loopback_speech_health(client: &Client, parsed: &Url, secret: Option<&str>) -> bool {
    let is_loopback = parsed.host().is_some_and(|host| match host {
        Host::Ipv4(ip) => ip.is_loopback(),
        Host::Ipv6(ip) => ip.is_loopback(),
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
    });
    if !is_loopback {
        return false;
    }
    let origin = parsed.origin().ascii_serialization();
    for path in ["/health", "/"] {
        if let Ok(response) = get(client, format!("{origin}{path}"), secret).send().await
            && response.status().is_success()
        {
            return true;
        }
    }
    false
}

pub(super) async fn probe_embedding(
    connection: &settings::Connection,
    credentials: &model::Credentials,
    providers: &Providers,
    configured_dim: u32,
) -> settings::Check {
    let checked_at = now();
    let Some(base) = settings::endpoint_of(connection, providers) else {
        return failed(checked_at, "接続先URLが正しくありません");
    };
    if Url::parse(&base).is_err() {
        return failed(checked_at, "接続先URLが正しくありません");
    }
    let secret = match model::connection_secret(connection, Some(credentials), providers) {
        Ok(secret) => secret,
        Err(error) => return failed(checked_at, error.to_string()),
    };

    let mut resources = Vec::new();
    if let Ok(client) = net::client()
        && let Ok(response) = get(&client, format!("{base}/models"), secret.as_deref())
            .send()
            .await
    {
        let status = response.status();
        if auth_failed(status) {
            return failed(checked_at, auth_message(status));
        }
        if status.is_success() {
            resources.extend(
                read_listed_model_ids(response)
                    .await
                    .into_iter()
                    .filter(|id| {
                        let lower = id.to_ascii_lowercase();
                        lower.contains("embed") || lower.contains("e5")
                    }),
            );
        }
    }
    if model::is_sakura_connection(&connection.endpoint().adapter, &base) {
        for id in model::SAKURA_EMBEDDING_MODELS {
            if !resources.iter().any(|known| known == id) {
                resources.push((*id).to_owned());
            }
        }
    }

    match crate::host::mind::embedding::probe_dimensions(connection, credentials, providers).await {
        Ok(actual_dim) => {
            let mut check = if configured_dim != 0 && configured_dim as usize != actual_dim {
                failed(
                    checked_at,
                    format!(
                        "埋め込み次元数が一致しません（設定: {configured_dim} / モデルの出力: {actual_dim}）。次元数を {actual_dim} に設定してください。"
                    ),
                )
            } else {
                healthy(checked_at, Vec::new())
            };
            check.capabilities.push(format!("dimensions:{actual_dim}"));
            check.resources = resources;
            check
        }
        Err(error) => settings::Check {
            resources,
            ..failed(checked_at, error.to_string())
        },
    }
}
