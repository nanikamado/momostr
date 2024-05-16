use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tracing::error;

#[derive(Debug, Clone)]
pub enum Error {
    Internal(Arc<anyhow::Error>),
    NotFound,
    NotFoundWithMsg(String),
    BadRequest(Option<String>),
}

impl<T> From<T> for Error
where
    T: Into<anyhow::Error>,
{
    fn from(t: T) -> Self {
        Error::Internal(Arc::new(t.into()))
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        match self {
            Error::Internal(e) => {
                error!("internal error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, format!("{}", e)).into_response()
            }
            Error::NotFound => (StatusCode::NOT_FOUND, "Not Found").into_response(),
            Error::NotFoundWithMsg(msg) => {
                (StatusCode::NOT_FOUND, format!("Not Found: {msg}")).into_response()
            }
            Error::BadRequest(None) => (StatusCode::BAD_REQUEST, "Bad Request").into_response(),
            Error::BadRequest(Some(msg)) => (StatusCode::BAD_REQUEST, msg).into_response(),
        }
    }
}
