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
        (
            self.status,
            Json(ene_rpc::ApiFailure {
                error: ene_rpc::Failure {
                    code: self.code.into(),
                    message: self.message.into(),
                    details: ene_rpc::JsonDocument {
                        json: self
                            .reason
                            .map(|r| json!({"reason": r}))
                            .unwrap_or(json!({}))
                            .to_string(),
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                }
                .into(),
                request_id: Uuid::new_v4().to_string(),
                http_status: u32::from(self.status.as_u16()),
                ..Default::default()
            }),
        )
            .into_response()
    }
}

impl From<ene_core::domain::Error> for ApiError {
    fn from(error: ene_core::domain::Error) -> Self {
        use ene_core::domain::ErrorKind;
        let mut result = match error.kind {
            ErrorKind::NotFound => Self::not_found(),
            ErrorKind::ResourceLimit => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "resource_limit",
                "応答または設定が容量上限を超えています",
            ),
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
impl ApiError {
    pub(crate) fn into_rpc(self) -> connectrpc::ConnectError {
        use connectrpc::ErrorCode;
        let code = match self.status.as_u16() {
            400 | 422 => ErrorCode::InvalidArgument,
            401 => ErrorCode::Unauthenticated,
            403 => ErrorCode::PermissionDenied,
            404 => ErrorCode::NotFound,
            409 => ErrorCode::FailedPrecondition,
            _ => ErrorCode::Unavailable,
        };
        let failure = ene_rpc::Failure {
            code: self.code.into(),
            message: self.message.into(),
            details: ene_rpc::JsonDocument {
                json: self
                    .reason
                    .map(|r| json!({"reason":r}))
                    .unwrap_or(json!({}))
                    .to_string(),
                ..Default::default()
            }
            .into(),
            ..Default::default()
        };
        connectrpc::ConnectError::new(code, self.message).with_detail(
            connectrpc::ErrorDetail::from_message(
                "ene.host.v1.ApiFailure",
                &ene_rpc::rpc::ApiFailure {
                    error: failure.into(),
                    request_id: Uuid::new_v4().to_string(),
                    http_status: u32::from(self.status.as_u16()),
                    ..Default::default()
                },
            ),
        )
    }
}
