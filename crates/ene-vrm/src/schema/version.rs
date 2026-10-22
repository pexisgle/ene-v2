use serde::Deserialize;

#[derive(Clone, Copy, Deserialize)]
pub(crate) enum Version {
    #[serde(rename = "1.0")]
    V1,
}
