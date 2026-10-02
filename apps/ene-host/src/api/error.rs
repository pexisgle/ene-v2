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
    reason: Option<&'static str>,
}
impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            reason: None,
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
        (self.status,Json(json!({"error":{"code":self.code,"message":self.message,"details":self.reason.map(|r|json!({"reason":r})).unwrap_or(json!({}))},"request_id":Uuid::new_v4().to_string()}))).into_response()
    }
}

impl From<ene_core::domain::Error> for ApiError {
    fn from(error: ene_core::domain::Error) -> Self {
        use ene_core::domain::ErrorKind;
        let mut result = match error.kind {
            ErrorKind::NotFound => Self::not_found(),
            ErrorKind::InvalidRequest => Self::invalid(),
            ErrorKind::RevisionConflict => Self::new(
                StatusCode::CONFLICT,
                "revision_conflict",
                "設定が更新されています。取得し直してください",
            ),
            ErrorKind::StateConflict => Self::new(
                StatusCode::CONFLICT,
                "state_conflict",
                "要求を完了できませんでした",
            ),
            ErrorKind::IdempotencyConflict => Self::new(
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "要求を完了できませんでした",
            ),
            ErrorKind::RequestInProgress => Self::new(
                StatusCode::CONFLICT,
                "request_in_progress",
                "要求を完了できませんでした",
            ),
            ErrorKind::PermissionRequired => Self::new(
                StatusCode::FORBIDDEN,
                "permission_required",
                "送信先・データ・用途への許可が必要です",
            ),
            ErrorKind::CredentialMissing => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "credential_missing",
                "APIキーを登録してください",
            ),
            ErrorKind::UnsupportedCapability => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_capability",
                "要求を完了できませんでした",
            ),
            ErrorKind::ResyncRequired => Self::new(
                StatusCode::CONFLICT,
                "resync_required",
                "通知購読からやり直してください",
            ),
            _ => Self::unavailable(),
        };
        result.reason = error.reason;
        result
    }
}
pub(super) fn wire<T: serde::de::DeserializeOwned>(
    value: impl serde::Serialize,
) -> Result<T, ApiError> {
    serde_json::from_value(serde_json::to_value(value).map_err(|_| ApiError::unavailable())?)
        .map_err(|_| ApiError::unavailable())
}

pub(super) fn response(
    (outcome, value): (ene_core::ports::Outcome, serde_json::Value),
) -> (StatusCode, Json<serde_json::Value>) {
    (
        match outcome {
            ene_core::ports::Outcome::Pending => StatusCode::CONFLICT,
            ene_core::ports::Outcome::Completed => StatusCode::OK,
            ene_core::ports::Outcome::Created => StatusCode::CREATED,
            ene_core::ports::Outcome::Accepted => StatusCode::ACCEPTED,
            ene_core::ports::Outcome::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        },
        Json(value),
    )
}

pub(super) fn command<T: serde::de::DeserializeOwned>(
    input: impl serde::Serialize,
) -> Result<T, ApiError> {
    serde_json::from_value(serde_json::to_value(input).map_err(|_| ApiError::invalid())?)
        .map_err(|_| ApiError::invalid())
}
