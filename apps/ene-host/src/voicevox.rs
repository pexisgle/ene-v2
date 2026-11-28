use anyhow::{Result, ensure};
use ene_rpc::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use wav_codec::{SampleFormat, WavReader};

#[derive(Clone)]
pub(crate) struct Engine {
    http: reqwest::Client,
    process: Arc<Mutex<Option<Process>>>,
}

struct Process {
    endpoint: String,
    child: Box<dyn process_wrap::tokio::ChildWrapper>,
}

#[derive(Deserialize)]
struct Speaker {
    name: String,
    styles: Vec<Style>,
}

#[derive(Deserialize)]
struct Style {
    id: u64,
    name: String,
}

impl Engine {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            process: Arc::default(),
        }
    }

    pub(crate) async fn speakers(&self, connection: &Connection) -> Result<Vec<String>> {
        let bytes = self.speaker_response(connection).await?;
        let speakers: Vec<Speaker> = serde_json::from_slice(&bytes)?;
        Ok(speakers
            .into_iter()
            .flat_map(|speaker| {
                speaker
                    .styles
                    .into_iter()
                    .map(move |style| format!("{}:{}（{}）", style.id, speaker.name, style.name))
            })
            .collect())
    }

    async fn speaker_response(&self, connection: &Connection) -> Result<Vec<u8>> {
        let base = connection.endpoint.trim_end_matches('/');
        let url = format!("{base}/speakers");
        let request = || self.http.get(&url).timeout(Duration::from_secs(2)).send();
        match request().await {
            Ok(response) => bounded(response, 2 * 1024 * 1024).await,
            Err(error) if error.is_connect() => {
                self.start_local(base).await?;
                bounded(request().await?, 2 * 1024 * 1024).await
            }
            Err(error) => Err(error.without_url().into()),
        }
    }

    async fn start_local(&self, base: &str) -> Result<()> {
        use process_wrap::tokio::{CommandWrap, KillOnDrop};

        let url = crate::model::check_endpoint(base)?;
        ensure!(
            url.scheme() == "http"
                && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]")),
            "only a loopback VOICEVOX engine can be started locally"
        );
        let mut process = self.process.lock().await;
        let version = format!("{base}/version");
        if self
            .http
            .get(&version)
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .is_ok_and(|reply| reply.status().is_success())
        {
            return Ok(());
        }
        if process.as_mut().is_none_or(|process| {
            process.endpoint != base || !matches!(process.child.try_wait(), Ok(None))
        }) {
            let program = engine_path()
                .ok_or_else(|| crate::error::missing("VOICEVOX engine not installed"))?;
            let port = url.port().unwrap_or(80).to_string();
            let host = url
                .host_str()
                .unwrap_or("127.0.0.1")
                .trim_matches(['[', ']']);
            let mut command = CommandWrap::with_new(&program, |command| {
                command
                    .args(["--host", host, "--port", &port])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Some(parent) = program.parent().filter(|path| !path.as_os_str().is_empty()) {
                    command.current_dir(parent);
                }
            });
            #[cfg(unix)]
            command.wrap(process_wrap::tokio::ProcessGroup::leader());
            #[cfg(windows)]
            command.wrap(process_wrap::tokio::JobObject);
            *process = Some(Process {
                endpoint: base.to_owned(),
                child: command.wrap(KillOnDrop).spawn()?,
            });
        }
        tokio::time::timeout(Duration::from_secs(25), async {
            loop {
                if self
                    .http
                    .get(&version)
                    .timeout(Duration::from_secs(1))
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success())
                {
                    return Ok(());
                }
                ensure!(
                    process
                        .as_mut()
                        .is_some_and(|process| matches!(process.child.try_wait(), Ok(None))),
                    "VOICEVOX engine exited"
                );
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        })
        .await?
    }

    pub(crate) async fn synthesize(
        &self,
        connection: &Connection,
        text: &str,
        voice: &str,
        speed: f32,
    ) -> Result<(Vec<u8>, u32)> {
        let base = connection.endpoint.trim_end_matches('/');
        let speaker = voice
            .split(':')
            .next()
            .unwrap_or("")
            .parse::<u64>()
            .map_err(|_| crate::error::invalid("VOICEVOX speaker must be a style ID"))?;
        let query_url = format!("{base}/audio_query");
        let query = || {
            self.http
                .post(&query_url)
                .query(&[("text", text), ("speaker", &speaker.to_string())])
                .timeout(Duration::from_secs(5))
                .send()
        };
        let response = match query().await {
            Err(error) if error.is_connect() => {
                self.start_local(base).await?;
                query().await?
            }
            other => other?,
        };
        let mut data: Value = serde_json::from_slice(&bounded(response, 2 * 1024 * 1024).await?)?;
        data["speedScale"] = speed.into();
        data["outputStereo"] = false.into();
        let response = self
            .http
            .post(format!("{base}/synthesis"))
            .query(&[("speaker", speaker)])
            .json(&data)
            .send()
            .await?;
        decode_wav(&bounded(response, 12 * 1024 * 1024).await?)
    }
}

async fn bounded(response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    let mut response = response
        .error_for_status()
        .map_err(reqwest::Error::without_url)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= max - bytes.len(),
            "VOICEVOX reply exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) fn decode_wav(bytes: &[u8]) -> Result<(Vec<u8>, u32)> {
    let mut reader = WavReader::new(bytes)?;
    let spec = reader.spec();
    ensure!(
        spec.sample_format == SampleFormat::Int
            && spec.bits_per_sample == 16
            && reader.spec_ex().bytes_per_sample == 2
            && spec.channels == 1
            && (8_000..=192_000).contains(&spec.sample_rate),
        "invalid mono PCM WAV format"
    );
    let mut out = Vec::with_capacity(bytes.len());
    for sample in reader.samples::<i16>() {
        out.extend_from_slice(&sample?.to_le_bytes());
    }
    Ok((out, spec.sample_rate))
}

fn engine_path() -> Option<PathBuf> {
    let mut paths: Vec<_> = ["ENE_VOICEVOX_ENGINE", "VOICEVOX_ENGINE_PATH"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    #[cfg(windows)]
    for dir in ["LOCALAPPDATA", "ProgramFiles"]
        .into_iter()
        .filter_map(std::env::var_os)
    {
        paths.push(PathBuf::from(dir).join("VOICEVOX/vv-engine/run.exe"));
    }
    #[cfg(not(windows))]
    paths.extend(
        [
            "/opt/voicevox/vv-engine/run",
            "/Applications/VOICEVOX.app/Contents/Resources/vv-engine/run",
        ]
        .map(PathBuf::from),
    );
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path).map(|dir| {
            dir.join(if cfg!(windows) {
                "voicevox_engine.exe"
            } else {
                "voicevox_engine"
            })
        }));
    }
    paths.into_iter().find(|path| path.is_file())
}
