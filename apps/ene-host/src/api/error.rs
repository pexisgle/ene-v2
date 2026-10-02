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

#[derive(Debug)]
pub struct Error {
    pub status: StatusCode,
    pub code: &'static str,
    pub reason: Option<&'static str>,
}
impl Error {
    pub fn new(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            reason: None,
        }
    }
    pub fn invalid() -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request")
    }
    pub fn from_store(error: crate::infrastructure::sqlite::Error) -> Self {
        match error {
            crate::infrastructure::sqlite::Error::IdempotencyConflict => {
                Self::new(StatusCode::CONFLICT, "idempotency_conflict")
            }
            crate::infrastructure::sqlite::Error::StateConflict => Self::new(StatusCode::CONFLICT, "state_conflict"),
            _ => Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        }
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.status,Json(json!({"error":{"code":self.code,"message":match self.code {"permission_required"=>"送信先・データ・用途への許可が必要です","credential_missing"=>"APIキーを登録してください","revision_conflict"=>"設定が更新されています。取得し直してください",_=>"要求を完了できませんでした"},"details":self.reason.map(|reason|json!({"reason":reason})).unwrap_or(json!({}))},"request_id":Uuid::new_v4().to_string()}))).into_response()
    }
}
