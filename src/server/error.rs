use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::harness::{ErrorKind, HarnessError};
use crate::openai::{ErrorBody, ErrorEnvelope};
use crate::prompt::RenderError;
use crate::routing::RouteError;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub code: Option<&'static str>,
    pub message: String,
    pub param: Option<String>,
}

impl ApiError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "invalid_request_error",
            code: None,
            message: message.into(),
            param: None,
        }
    }

    pub fn unsupported(param: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            kind: "invalid_request_error",
            code: Some("unsupported_parameter"),
            message: format!("`{param}` is not supported by locum"),
            param: Some(param.to_string()),
        }
    }

    pub fn model_not_found(model: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            kind: "invalid_request_error",
            code: Some("model_not_found"),
            message: format!("The model `{model}` does not exist or is not enabled"),
            param: Some("model".into()),
        }
    }

    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            kind: "authentication_error",
            code: Some("invalid_api_key"),
            message: "Incorrect API key provided".into(),
            param: None,
        }
    }

    pub fn busy() -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            kind: "rate_limit_error",
            code: Some("server_busy"),
            message: "locum is at capacity; retry shortly".into(),
            param: None,
        }
    }

    pub fn envelope(&self) -> ErrorEnvelope {
        ErrorEnvelope {
            error: ErrorBody {
                message: self.message.clone(),
                kind: self.kind,
                param: self.param.clone(),
                code: self.code,
            },
        }
    }
}

impl From<HarnessError> for ApiError {
    fn from(e: HarnessError) -> Self {
        let (status, kind, code) = match e.kind {
            ErrorKind::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
                "harness_unavailable",
            ),
            ErrorKind::Failed => (StatusCode::BAD_GATEWAY, "api_error", "harness_error"),
            ErrorKind::Timeout => (StatusCode::GATEWAY_TIMEOUT, "api_error", "harness_timeout"),
            ErrorKind::ContextLength => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "context_length_exceeded",
            ),
            ErrorKind::Interrupted => (StatusCode::BAD_GATEWAY, "api_error", "harness_interrupted"),
            ErrorKind::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "harness_rate_limited",
            ),
            ErrorKind::BadRequest => (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "harness_rejected",
            ),
        };
        Self {
            status,
            kind,
            code: Some(code),
            message: e.message,
            param: None,
        }
    }
}

impl From<RouteError> for ApiError {
    fn from(e: RouteError) -> Self {
        match e {
            RouteError::Unknown(m) => ApiError::model_not_found(&m),
            RouteError::Disabled(k) => ApiError {
                status: StatusCode::NOT_FOUND,
                kind: "invalid_request_error",
                code: Some("model_not_found"),
                message: format!("harness `{k}` is disabled in the locum config"),
                param: Some("model".into()),
            },
        }
    }
}

impl From<RenderError> for ApiError {
    fn from(e: RenderError) -> Self {
        match e {
            RenderError::Tools => ApiError::unsupported("tools"),
            RenderError::BadPart => ApiError::unsupported("messages[].content[].type"),
            other => {
                let mut a = ApiError::invalid(other.to_string());
                a.param = Some("messages".into());
                a
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.envelope())).into_response()
    }
}

pub fn json_rejection(err: axum::extract::rejection::JsonRejection) -> ApiError {
    ApiError::invalid(format!("invalid JSON body: {}", err.body_text()))
}
