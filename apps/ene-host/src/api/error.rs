use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use uuid::Uuid;

pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}
impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }
    pub(crate) fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "ホストの保存処理を利用できません",
        )
    }
    pub(crate) fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "対象が見つかりません")
    }
    pub(crate) fn invalid() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "要求の形式が正しくありません",
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ene_api_types::ApiErrorBody {
                error: ene_api_types::Failure {
                    code: self.code.into(),
                    message: self.message.into(),
                    details: json!({}),
                },
                request_id: Uuid::new_v4().to_string(),
            }),
        )
            .into_response()
    }
}

impl From<ene_core::domain::Error> for ApiError {
    fn from(error: ene_core::domain::Error) -> Self {
        use ene_core::domain::ErrorKind;
        match error.kind {
            ErrorKind::NotFound => Self::not_found(),
            ErrorKind::InvalidRequest => Self::invalid(),
            ErrorKind::ResyncRequired => Self::new(
                StatusCode::CONFLICT,
                "resync_required",
                "通知購読からやり直してください",
            ),
            _ => Self::unavailable(),
        }
    }
}
pub(super) fn wire<T: serde::de::DeserializeOwned>(
    value: impl serde::Serialize,
) -> Result<T, ApiError> {
    serde_json::from_value(serde_json::to_value(value).map_err(|_| ApiError::unavailable())?)
        .map_err(|_| ApiError::unavailable())
}
