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
    let value: RuntimeConnection = serde_json::from_reader(std::io::Read::take(file, 8193))
        .map_err(|_| "接続情報を読めません")?;
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
impl From<ene_api_client::Error> for Error {
    fn from(error: ene_api_client::Error) -> Self {
        match error {
            ene_api_client::Error::Unauthorized => Self::Unauthorized,
            ene_api_client::Error::Api(error) if error.body.error.code == "revision_conflict" => {
                Self::Api("設定が変わったため保存を完了できませんでした。「元に戻す」で最新の設定と送信先を確認し、編集・許可をやり直してください。".into())
            }
            ene_api_client::Error::Api(error) => Self::Api(error.to_string()),
            ene_api_client::Error::ResourceLimit => Self::Api("会話の読み込み量が上限を超えました。再接続して必要な履歴だけを読み込んでください。".into()),
            ene_api_client::Error::Busy => Self::Api("要求が多すぎます。処理の完了後に再試行してください。".into()),
            ene_api_client::Error::ResyncRequired => Self::Api("接続先が変わりました。再接続して内容を確認してください。".into()),
            _ => Self::Transport,
        }
    }
}
