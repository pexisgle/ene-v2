use super::*;
pub(super) fn descriptor(path: &Path) -> Result<RuntimeConnection, String> {
    // Inspect the opened file rather than a potentially replaced path.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(path)
        .map_err(|_| "このPCのホストが起動していません")?;
    let meta = file.metadata().map_err(|_| "接続情報を確認できません")?;
    if !meta.is_file()
        || meta.uid() != rustix::process::geteuid().as_raw()
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() > 8192
    {
        return Err("接続情報の所有者・権限・形式が不正です".into());
    }
    let value: RuntimeConnection =
        serde_json::from_reader(file).map_err(|_| "接続情報を読めません")?;
    let url = url::Url::parse(&value.endpoint).map_err(|_| "接続先が不正です")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || url.path() != "/v1"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || value.token.is_empty()
        || value.token.chars().any(char::is_control)
    {
        return Err("ループバックのホストだけに接続できます".into());
    }
    Ok(value)
}
#[derive(Clone)]
pub(super) struct Http {
    pub(super) client: Client,
    pub(super) connection: std::sync::Arc<RuntimeConnection>,
}
#[derive(Debug)]
pub(super) enum Error {
    Transport,
    Unauthorized,
    Api(String),
}
impl Http {
    pub(super) async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        key: Option<&str>,
    ) -> Result<Value, Error> {
        let mut request = self
            .client
            .request(method, format!("{}{}", self.connection.endpoint, path))
            .bearer_auth(&self.connection.token)
            .timeout(Duration::from_secs(if path.ends_with("/check") {
                95
            } else {
                5
            }));
        if let Some(body) = body {
            request = request.json(&body);
        }
        if let Some(key) = key {
            request = request.header("Idempotency-Key", key);
        }
        let response = request.send().await.map_err(|_| Error::Transport)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        if status == reqwest::StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        let body: Value = response.json().await.map_err(|_| Error::Transport)?;
        if !status.is_success() {
            return Err(Error::Api(format!(
                "{} ({})",
                body["error"]["message"]
                    .as_str()
                    .unwrap_or("要求を完了できません"),
                body["error"]["code"].as_str().unwrap_or("unavailable")
            )));
        }
        Ok(body)
    }
    pub(super) async fn snapshot(&self, id: &str) -> Result<Snapshot, Error> {
        serde_json::from_value(
            self.request(
                Method::GET,
                &format!("/snapshot?connection_id={id}"),
                None,
                None,
            )
            .await?,
        )
        .map_err(|_| Error::Transport)
    }
}
