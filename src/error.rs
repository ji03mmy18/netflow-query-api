//! API 錯誤型別。
//!
//! 回應格式固定為 `{"error": {"code": ..., "message": ...}}`，code 是
//! 給程式判斷用的穩定字串，message 是給人看的中文說明。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("API Key 無效或未提供")]
    Unauthorized,

    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    Validation(String),

    #[error("資料庫查詢失敗")]
    Database(#[from] sqlx::Error),
}

impl ApiError {
    pub fn validation(msg: impl Into<String>) -> Self {
        Self::Validation(msg.into())
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }

    fn parts(&self) -> (StatusCode, &'static str) {
        match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED"),
            Self::Forbidden(_) => (StatusCode::FORBIDDEN, "FORBIDDEN"),
            Self::Validation(_) => (StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_ERROR"),
            Self::Database(_) => (StatusCode::INTERNAL_SERVER_ERROR, "DATABASE_ERROR"),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.parts();

        // 資料庫錯誤的細節只進日誌，不外流到回應：連線字串、資料表結構
        // 這類資訊對呼叫端沒有用處，對攻擊者才有。
        let message = match &self {
            Self::Database(source) => {
                tracing::error!(error = %source, "database query failed");
                self.to_string()
            }
            _ => self.to_string(),
        };

        (status, Json(json!({ "error": { "code": code, "message": message } }))).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
