//! Body, path and query extractors whose rejections answer with the
//! contract's error body (`400 invalid_request`) instead of axum's plain
//! text.

use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::answer::Failure;

/// A JSON body. Unknown fields are refused by the request types themselves.
pub struct Body<T>(pub T);

impl<State, T> FromRequest<State> for Body<T>
where
    State: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Failure;

    async fn from_request(request: Request, state: &State) -> Result<Self, Self::Rejection> {
        axum::Json::<T>::from_request(request, state)
            .await
            .map(|axum::Json(value)| Self(value))
            .map_err(|rejection: JsonRejection| Failure::invalid(rejection.body_text()))
    }
}

/// Path parameters.
pub struct Params<T>(pub T);

impl<State, T> FromRequestParts<State> for Params<T>
where
    State: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, state: &State) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(value)| Self(value))
            .map_err(|rejection: PathRejection| Failure::invalid(rejection.body_text()))
    }
}

/// Query string parameters.
pub struct Query<T>(pub T);

impl<State, T> FromRequestParts<State> for Query<T>
where
    State: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, state: &State) -> Result<Self, Self::Rejection> {
        axum::extract::Query::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Query(value)| Self(value))
            .map_err(|rejection: QueryRejection| Failure::invalid(rejection.body_text()))
    }
}
