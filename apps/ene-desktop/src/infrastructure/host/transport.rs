use super::*;
pub(super) fn descriptor(path: &Path) -> Result<RuntimeConnection, String> {
    #[cfg(unix)]
    let file = {
        // Inspect the opened file rather than a potentially replaced path.
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(
                rustix::fs::OFlags::NOFOLLOW.bits() as i32
                    | rustix::fs::OFlags::NONBLOCK.bits() as i32,
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
        file
    };
    #[cfg(windows)]
    let file = ene_platform::storage::open(path, false, true)
        .map_err(|_| "接続情報の所有者・権限・形式が不正です")?;
    let descriptor: serde_json::Value = serde_json::from_reader(std::io::Read::take(file, 8193))
        .map_err(|_| "接続情報を読めません")?;
    let value = RuntimeConnection {
        endpoint: descriptor["endpoint"]
            .as_str()
            .ok_or("接続情報を読めません")?
            .to_owned(),
        token: descriptor["token"]
            .as_str()
            .ok_or("接続情報を読めません")?
            .to_owned(),
    };
    let url = url::Url::parse(&value.endpoint).map_err(|_| "接続先が不正です")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || url.path() != "/rpc"
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
#[derive(Debug)]
pub(super) enum Error {
    Transport,
    Unauthorized,
    Api(String),
    PartialConfiguration {
        connection: String,
        error: Box<Error>,
    },
}

pub struct RuntimeConnection {
    pub endpoint: String,
    pub token: String,
}
pub(super) fn connect(connection: &RuntimeConnection) -> Result<Http, connectrpc::ConnectError> {
    let (transport, config) = ene_rpc::connect(&connection.endpoint, &connection.token)?;
    Ok(ene_rpc::HostServiceClient::new(transport, config))
}
pub(super) fn api_failure(error: &connectrpc::ConnectError) -> Option<ene_rpc::ApiFailure> {
    use base64::Engine;
    error.details.iter().find_map(|detail| {
        if !detail.type_url.ends_with("ene.host.v1.ApiFailure") {
            return None;
        }
        let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(detail.value.as_ref()?)
            .ok()?;
        let failure = <ene_rpc::ApiFailure as buffa::Message>::decode_from_slice(&bytes).ok()?;
        failure.error.is_set().then_some(failure)
    })
}
impl From<connectrpc::ConnectError> for Error {
    fn from(error: connectrpc::ConnectError) -> Self {
        use connectrpc::ErrorCode;
        if error.code == ErrorCode::Unauthenticated {
            return Self::Unauthorized;
        }
        if let Some(failure) = api_failure(&error) {
            return if failure.error.code == "revision_conflict" {
                Self::Api("設定が変わったため保存を完了できませんでした。「元に戻す」で最新の設定と送信先を確認し、編集・許可をやり直してください。".into())
            } else {
                Self::Api(format!(
                    "{} ({})",
                    failure.error.message, failure.error.code
                ))
            };
        }
        match error.code {
            ErrorCode::ResourceExhausted => Self::Api(
                "要求または読み込み量が上限を超えました。再接続して再試行してください。".into(),
            ),
            ErrorCode::FailedPrecondition => {
                Self::Api("接続先または状態が変わりました。取得し直してください。".into())
            }
            ErrorCode::PermissionDenied => Self::Api("操作への許可が必要です".into()),
            ErrorCode::NotFound => Self::Api("対象が見つかりません".into()),
            ErrorCode::InvalidArgument => Self::Api("要求の形式が正しくありません".into()),
            ErrorCode::Unimplemented => Self::Api("この機能を利用できません".into()),
            _ => Self::Transport,
        }
    }
}
