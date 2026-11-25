use crate::host::{model, settings};

pub(super) async fn probe_voicevox_tts(
    connection: &crate::host::settings::Connection,
    voicevox: &crate::host::voice::voicevox::VoicevoxManager,
) -> crate::host::settings::Check {
    let checked_at = crate::host::util::now();
    let base = connection.endpoint().endpoint.trim().trim_end_matches('/');
    let base = if base.is_empty() {
        "http://127.0.0.1:50021"
    } else {
        base
    };
    let url = format!("{base}/speakers");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return crate::host::settings::Check {
                healthy: false,
                checked_at,
                failure: Some(crate::host::error::Failure::new(
                    "connection_check",
                    error.to_string(),
                )),
                capabilities: Vec::new(),
                resources: Vec::new(),
            };
        }
    };

    let mut response = client.get(&url).send().await;
    if response.is_err()
        && let Err(start_error) = voicevox
            .ensure_running(base, &connection.endpoint().options)
            .await
    {
        return crate::host::settings::Check {
            healthy: false,
            checked_at,
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                start_error.to_string(),
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        };
    } else if response.is_err() {
        response = client.get(&url).send().await;
    }

    match response {
        Ok(response) if response.status().is_success() => {
            let mut resources = Vec::new();
            if let Ok(speakers) = response.json::<Vec<serde_json::Value>>().await {
                for speaker in speakers {
                    let speaker_name = speaker
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map_or("", str::trim);
                    if let Some(styles) = speaker.get("styles").and_then(|s| s.as_array()) {
                        for style in styles {
                            if let Some(id) = style.get("id").and_then(serde_json::Value::as_u64) {
                                let style_name = style
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .map_or("", str::trim);
                                let entry = match (speaker_name.is_empty(), style_name.is_empty()) {
                                    (false, false) => {
                                        format!("{id}:{speaker_name}（{style_name}）")
                                    }
                                    (false, true) => format!("{id}:{speaker_name}"),
                                    _ => id.to_string(),
                                };
                                resources.push(entry);
                            }
                        }
                    }
                }
            }
            if resources.is_empty() {
                resources.extend([
                    "1:ずんだもん（あまあま）".to_owned(),
                    "3:ずんだもん（ノーマル）".to_owned(),
                ]);
            }
            crate::host::settings::Check {
                healthy: true,
                checked_at,
                failure: None,
                capabilities: Vec::new(),
                resources,
            }
        }
        Ok(response) => crate::host::settings::Check {
            healthy: false,
            checked_at,
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                format!("HTTP {}", response.status().as_u16()),
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        },
        Err(error) => crate::host::settings::Check {
            healthy: false,
            checked_at,
            failure: Some(crate::host::error::Failure::new(
                "connection_check",
                error.to_string(),
            )),
            capabilities: Vec::new(),
            resources: Vec::new(),
        },
    }
}

pub(super) async fn probe_openai_speech(
    connection: &crate::host::settings::Connection,
    credentials: &model::Credentials,
    providers: &crate::host::providers::Providers,
) -> crate::host::settings::Check {
    let checked_at = crate::host::util::now();
    let fail = |message: String| speech_probe_failure(&checked_at, message);
    let base = settings::endpoint_of(connection, providers).unwrap_or_else(|| {
        connection
            .endpoint()
            .endpoint
            .trim()
            .trim_end_matches('/')
            .to_owned()
    });
    let Ok(parsed) = url::Url::parse(&base) else {
        return fail("接続先URLが正しくありません".into());
    };
    let sakura = model::is_sakura_connection(&connection.endpoint().adapter, &base);
    let secret = match model::connection_secret(connection, Some(credentials), providers) {
        Ok(secret) => secret,
        Err(error) => return fail(error.to_string()),
    };
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => return fail(error.to_string()),
    };
    let models_url = if sakura && base.ends_with("/tts/v1") {
        format!("{}/v1/models", base.trim_end_matches("/tts/v1"))
    } else {
        format!("{base}/models")
    };
    let mut req = client.get(&models_url);
    if let Some(key) = secret.as_deref() {
        req = req.bearer_auth(key);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            let listed = read_listed_model_ids(resp).await;
            crate::host::settings::Check {
                healthy: true,
                checked_at,
                failure: None,
                capabilities: Vec::new(),
                resources: collect_speech_resources(connection.kind, sakura, listed),
            }
        }
        Ok(resp) if matches!(resp.status().as_u16(), 401 | 403) => fail(format!(
            "認証エラー (HTTP {})。APIキー（アカウントトークン）を確認してください。",
            resp.status().as_u16()
        )),
        Ok(resp) => {
            if let Some(ok) =
                try_loopback_speech_health(&client, &parsed, secret.as_deref(), &checked_at).await
            {
                return ok;
            }
            fail(format!("HTTP {}", resp.status().as_u16()))
        }
        Err(error) => fail(error.to_string()),
    }
}

fn speech_probe_failure(checked_at: &str, message: String) -> crate::host::settings::Check {
    crate::host::settings::Check {
        healthy: false,
        checked_at: checked_at.to_owned(),
        failure: Some(crate::host::error::Failure::new(
            "connection_check",
            message,
        )),
        capabilities: Vec::new(),
        resources: Vec::new(),
    }
}

async fn read_listed_model_ids(resp: reqwest::Response) -> Vec<String> {
    let mut listed = Vec::new();
    if let Ok(json) = resp.json::<serde_json::Value>().await
        && let Some(items) = json.get("data").and_then(|v| v.as_array())
    {
        for item in items {
            if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                listed.push(id.to_owned());
            }
        }
    }
    listed
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

async fn try_loopback_speech_health(
    client: &reqwest::Client,
    parsed: &url::Url,
    secret: Option<&str>,
    checked_at: &str,
) -> Option<crate::host::settings::Check> {
    let is_loopback = parsed.host().is_some_and(|h| match h {
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
        url::Host::Domain(d) => d.eq_ignore_ascii_case("localhost"),
    });
    if !is_loopback {
        return None;
    }
    let origin = format!(
        "{}://{}{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or("127.0.0.1"),
        parsed.port().map_or_default(|p| format!(":{p}"))
    );
    for path in ["/health", "/"] {
        let mut fallback = client.get(format!("{origin}{path}"));
        if let Some(key) = secret {
            fallback = fallback.bearer_auth(key);
        }
        if let Ok(r) = fallback.send().await
            && r.status().is_success()
        {
            return Some(crate::host::settings::Check {
                healthy: true,
                checked_at: checked_at.to_owned(),
                failure: None,
                capabilities: Vec::new(),
                resources: Vec::new(),
            });
        }
    }
    None
}

pub(super) async fn probe_embedding(
    connection: &crate::host::settings::Connection,
    credentials: &model::Credentials,
    providers: &crate::host::providers::Providers,
) -> crate::host::settings::Check {
    let checked_at = crate::host::util::now();
    let fail = |message: String, resources: Vec<String>| crate::host::settings::Check {
        healthy: false,
        checked_at: checked_at.clone(),
        failure: Some(crate::host::error::Failure::new(
            "connection_check",
            message,
        )),
        capabilities: Vec::new(),
        resources,
    };
    let Some(base) = settings::endpoint_of(connection, providers) else {
        return fail("接続先URLが正しくありません".into(), Vec::new());
    };
    if url::Url::parse(&base).is_err() {
        return fail("接続先URLが正しくありません".into(), Vec::new());
    }
    let sakura = model::is_sakura_connection(&connection.endpoint().adapter, &base);
    let secret = match model::connection_secret(connection, Some(credentials), providers) {
        Ok(secret) => secret,
        Err(error) => return fail(error.to_string(), Vec::new()),
    };

    let mut resources = Vec::new();
    if let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        let mut req = client.get(format!("{base}/models"));
        if let Some(key) = secret.as_deref() {
            req = req.bearer_auth(key);
        }
        if let Ok(resp) = req.send().await {
            if matches!(resp.status().as_u16(), 401 | 403) {
                return fail(
                    format!(
                        "認証エラー (HTTP {})。APIキー（アカウントトークン）を確認してください。",
                        resp.status().as_u16()
                    ),
                    resources,
                );
            }
            if resp.status().is_success()
                && let Ok(json) = resp.json::<serde_json::Value>().await
                && let Some(items) = json.get("data").and_then(|v| v.as_array())
            {
                for item in items {
                    if let Some(id) = item.get("id").and_then(|v| v.as_str()) {
                        let lower = id.to_ascii_lowercase();
                        if lower.contains("embed") || lower.contains("e5") {
                            resources.push(id.to_owned());
                        }
                    }
                }
            }
        }
    }
    if sakura {
        for id in model::SAKURA_EMBEDDING_MODELS {
            if !resources.iter().any(|existing| existing == id) {
                resources.push((*id).to_owned());
            }
        }
    }

    match crate::host::mind::embedding::probe_dimensions(connection, credentials, providers).await {
        Ok(actual_dim) => {
            let configured_dim = match &connection.config {
                settings::ConnectionConfig::Embedding(cfg) => cfg.dimensions as usize,
                _ => 0,
            };
            if configured_dim > 0 && configured_dim != actual_dim {
                return crate::host::settings::Check {
                    healthy: false,
                    checked_at,
                    failure: Some(crate::host::error::Failure::new(
                        "connection_check",
                        format!(
                            "埋め込み次元数が一致しません（設定: {configured_dim} / モデルの出力: {actual_dim}）。次元数を {actual_dim} に設定してください。"
                        ),
                    )),
                    capabilities: vec![format!("dimensions:{actual_dim}")],
                    resources,
                };
            }
            crate::host::settings::Check {
                healthy: true,
                checked_at,
                failure: None,
                capabilities: vec![format!("dimensions:{actual_dim}")],
                resources,
            }
        }
        Err(error) => fail(error.to_string(), resources),
    }
}
