use crate::api::error::ApiError;
use axum::{extract::FromRequestParts, http::request::Parts};
use serde::de::DeserializeOwned;

/// Query extraction with the existing API error envelope.
pub(super) struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|query| Self(query.0))
            .map_err(|_| ApiError::invalid())
    }
}

/// Path failures use the same envelope as JSON/query failures.
pub(super) struct ApiPath<T>(pub T);
impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|path| Self(path.0))
            .map_err(|_| ApiError::invalid())
    }
}
