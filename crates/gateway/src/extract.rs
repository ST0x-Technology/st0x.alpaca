//! Body, path and query extractors whose rejections answer with the
//! contract's error body (`400 invalid_request`) instead of axum's plain
//! text. A rejection on an operation route is finalized like any other
//! refusal: `not_applied` on a mutation, the call's request id, and an
//! answered audit record.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::answer::Failure;
use crate::state::{AppState, Call, Intent};

/// Finalizes an extractor rejection for the call behind `parts`.
fn refuse(parts: &mut Parts, state: &AppState, message: String) -> Failure {
    match Call::of(parts) {
        Ok(call) => state.refuse(&call, &Intent::default(), Failure::invalid(message)),
        Err(failure) => failure,
    }
}

/// A JSON body. Unknown fields are refused by the request types themselves.
pub struct Body<T>(pub T);

impl<T> FromRequest<AppState> for Body<T>
where
    T: DeserializeOwned,
{
    type Rejection = Failure;

    async fn from_request(request: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let (mut parts, body) = request.into_parts();
        // Resolved before the body is consumed, so a rejection below still
        // knows its call.
        let call = Call::of(&mut parts)?;
        axum::Json::<T>::from_request(Request::from_parts(parts, body), state)
            .await
            .map(|axum::Json(value)| Self(value))
            .map_err(|rejection: JsonRejection| {
                state.refuse(
                    &call,
                    &Intent::default(),
                    Failure::invalid(rejection.body_text()),
                )
            })
    }
}

/// Path parameters.
pub struct Params<T>(pub T);

impl<T> FromRequestParts<AppState> for Params<T>
where
    T: DeserializeOwned + Send,
{
    type Rejection = Failure;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(Self(value)),
            Err(rejection) => Err(refuse(parts, state, rejection.body_text())),
        }
    }
}

/// Query string parameters.
pub struct Query<T>(pub T);

impl<T> FromRequestParts<AppState> for Query<T>
where
    T: DeserializeOwned,
{
    type Rejection = Failure;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(rejection) => Err(refuse(parts, state, rejection.body_text())),
        }
    }
}
