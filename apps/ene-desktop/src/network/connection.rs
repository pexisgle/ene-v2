use super::*;

#[derive(Deserialize)]
struct Discovery {
    endpoint: String,
    token: String,
}

#[derive(Clone)]
pub(super) struct Clients {
    pub(super) state: w::HostServiceClient<HttpClient>,
    pub(super) chat: w::ChatServiceClient<HttpClient>,
    pub(super) work: w::WorkServiceClient<HttpClient>,
    pub(super) settings: w::SettingsServiceClient<HttpClient>,
    pub(super) device: w::ClientServiceClient<HttpClient>,
    pub(super) voice: w::VoiceServiceClient<HttpClient>,
}

impl Clients {
    pub(super) fn connect(file: &Path) -> Result<(Self, bool)> {
        let handle = std::fs::File::open(file)?;
        private_fs::verify(&handle)?;
        let Discovery { endpoint, token } = serde_json::from_reader(handle)?;
        let local = endpoint.starts_with("http://");
        let (transport, config) = if local {
            w::connect(&endpoint, &token)?
        } else {
            w::connect_remote(&endpoint, &token)?
        };
        Ok((Self::new(transport, config), local))
    }

    fn new(transport: HttpClient, config: ClientConfig) -> Self {
        Self {
            state: w::HostServiceClient::new(
                transport.clone(),
                config
                    .clone()
                    .with_default_max_message_size(48 * 1024 * 1024),
            ),
            chat: w::ChatServiceClient::new(transport.clone(), config.clone()),
            work: w::WorkServiceClient::new(transport.clone(), config.clone()),
            settings: w::SettingsServiceClient::new(transport.clone(), config.clone()),
            device: w::ClientServiceClient::new(transport.clone(), config.clone()),
            voice: w::VoiceServiceClient::new(transport, config),
        }
    }
}

#[derive(Default)]
pub(super) struct LocalHost {
    pub(super) child: Option<std::process::Child>,
    attempted: Option<std::time::Instant>,
}

impl LocalHost {
    pub(super) fn ensure_running(&mut self, executable: &Path, discovery: &Path) {
        if self
            .child
            .as_mut()
            .is_some_and(|process| matches!(process.try_wait(), Ok(None)))
            || self
                .attempted
                .is_some_and(|start| start.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.attempted = Some(std::time::Instant::now());
        let mut command = std::process::Command::new(executable);
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        if let Some(runtime) = discovery.parent() {
            command.env("ENE_HOST_RUNTIME_DIR", runtime);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        match command.spawn() {
            Ok(process) => self.child = Some(process),
            Err(_) => tracing::warn!("local host could not be started"),
        }
    }
}
