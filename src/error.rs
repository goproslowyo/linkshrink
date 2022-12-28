use axum::{http::StatusCode,
           response::{IntoResponse, Response}};

pub type AppResult<T> = Result<T, ErrorWrapper>;

#[derive(Debug)]
pub struct ErrorWrapper(anyhow::Error);

impl<E> From<E> for ErrorWrapper where E: Into<anyhow::Error>
{
    fn from(value: E) -> Self {
        Self(value.into())
    }
}

impl IntoResponse for ErrorWrapper {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0)
        ).into_response()
    }
}
