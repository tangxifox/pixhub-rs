use axum::body::Body;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

#[derive(Debug)]
pub struct AppError {
    pub status_code: StatusCode,
    pub error_code: &'static str,
    pub message: String,
    pub details: Value,
    pub storage_name: Option<String>,
}

impl AppError {
    pub fn new(
        status_code: StatusCode,
        error_code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status_code,
            error_code,
            message: message.into(),
            details: json!({}),
            storage_name: None,
        }
    }

    pub fn with_details(
        status_code: StatusCode,
        error_code: &'static str,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        Self {
            status_code,
            error_code,
            message: message.into(),
            details,
            storage_name: None,
        }
    }

}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "error": true,
            "code": self.error_code,
            "message": self.message,
            "timestamp": chrono::Local::now().format("%Y/%m/%d %H:%M:%S").to_string(),
            "details": self.details,
        });

        if let Some(name) = &self.storage_name {
            body["storage"] = json!(name);
        }

        let json = serde_json::to_string_pretty(&body).unwrap_or_default();
        Response::builder()
            .status(self.status_code)
            .header("Content-Type", "application/json; charset=utf-8")
            .body(Body::from(json))
            .unwrap()
    }
}
