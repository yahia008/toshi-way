//! API errors. Every failure is `{"error": {"code": ..., "message": ...}}`.

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use escrow_core::{adapter::AdapterError, service::ServiceError, store::StoreError};
use serde_json::json;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub retry_after: Option<u64>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), retry_after: None }
    }
    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }
    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "a valid bearer token is required")
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }
    /// Also used when the caller has no right to know the escrow exists.
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "no such escrow")
    }
    pub fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": { "code": self.code, "message": self.message } }));
        let mut res = (self.status, body).into_response();
        if let Some(secs) = self.retry_after {
            res.headers_mut().insert(header::RETRY_AFTER, secs.into());
        }
        res
    }
}

impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        use ServiceError as E;
        match e {
            E::NotFound => Self::not_found(),
            E::Invalid(m) => Self::bad_request("invalid_request", m),
            E::Timing(t) => Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_deadline", t.to_string()),
            E::InvalidState { action, state } => Self::new(
                StatusCode::CONFLICT,
                "invalid_state",
                format!("cannot {action} an escrow in state {state}"),
            ),
            E::WrongPreimage => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, "wrong_preimage", "preimage does not match the payment hash")
            }
            E::TooLate => Self::new(StatusCode::CONFLICT, "too_late", "the settle-by deadline has passed"),
            E::TooEarly(at) => Self::new(
                StatusCode::CONFLICT,
                "response_window",
                format!("the other side has until {at} (unix time) to respond to the dispute; you can decide then, or as soon as they add their side"),
            ),
            E::HashInUse => Self::new(StatusCode::CONFLICT, "hash_in_use", "payment hash is already used by another escrow"),
            E::Adapter(AdapterError::Unavailable(_)) => {
                Self::new(StatusCode::SERVICE_UNAVAILABLE, "node_unavailable", "the Lightning node is unavailable, try again")
            }
            E::Adapter(AdapterError::NoNode(m)) => Self::new(StatusCode::CONFLICT, "no_node", m),
            E::Adapter(AdapterError::Rejected(_)) => {
                Self::new(StatusCode::BAD_GATEWAY, "node_rejected", "the Lightning node rejected the request")
            }
            E::Store(StoreError::Conflict) => {
                Self::new(StatusCode::CONFLICT, "conflict", "the escrow was changed concurrently, retry")
            }
            E::Store(err) => {
                tracing::error!(error = %err, "store failure");
                Self::internal()
            }
        }
    }
}

impl From<AdapterError> for ApiError {
    fn from(e: AdapterError) -> Self {
        ServiceError::from(e).into()
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        ServiceError::from(e).into()
    }
}
