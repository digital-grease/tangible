// SPDX-FileCopyrightText: 2026 digitalgrease
// SPDX-License-Identifier: AGPL-3.0-or-later

//! A request identifier, on every response and every log line about it.
//!
//! An error on an operator's screen used to carry a code and nothing that
//! tied it to the server's log. Now every request gets an identifier: it is
//! in the trace span the log lines sit in, in the `X-Request-Id` response
//! header, and in every problem document as `request_id`, so "the import
//! failed with INTERNAL" can be matched to the one log line that says why.
//!
//! Always this server's own UUIDv7. One a client sends is replaced, so
//! nobody can choose what the server's logs say a request was called.

use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

/// The header the identifier travels in, on the request as the trace sees
/// it and on the response.
pub const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

tokio::task_local! {
    /// The identifier of the request being handled on this task.
    static REQUEST_ID: String;
}

/// Give the request an identifier and put it on the response.
///
/// Outermost, so the trace and everything inside it see the identifier.
pub async fn assign(mut request: Request, next: Next) -> Response {
    let id = uuid::Uuid::now_v7().to_string();
    if let Ok(value) = HeaderValue::from_str(&id) {
        request.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    let mut response = REQUEST_ID.scope(id.clone(), next.run(request)).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

/// The identifier of the request being handled, if this task is handling
/// one.
#[must_use]
pub fn current() -> Option<String> {
    REQUEST_ID.try_with(Clone::clone).ok()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use tower::ServiceExt as _;

    use super::*;
    use crate::problem::{ErrorCode, Problem};

    fn app() -> Router {
        Router::new()
            .route(
                "/fails",
                get(|| async { Problem::new(ErrorCode::Internal, "it broke") }),
            )
            .route("/works", get(|| async { "fine" }))
            .layer(axum::middleware::from_fn(assign))
    }

    async fn call(
        path: &str,
        sent: Option<&str>,
    ) -> (StatusCode, Option<String>, serde_json::Value) {
        let mut request = axum::http::Request::get(path);
        if let Some(sent) = sent {
            request = request.header("x-request-id", sent);
        }
        let response = app()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let header = response
            .headers()
            .get("x-request-id")
            .map(|value| value.to_str().unwrap().to_owned());
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, header, body)
    }

    #[tokio::test]
    async fn an_error_names_the_request_it_answered() {
        let (status, header, body) = call("/fails", None).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let header = header.expect("a request id header");
        let id = uuid::Uuid::parse_str(&header).expect("a UUID");
        assert_eq!(id.get_version_num(), 7);
        assert_eq!(body["request_id"], header.as_str());
    }

    #[tokio::test]
    async fn every_response_has_one_and_each_is_different() {
        let (_, first, _) = call("/works", None).await;
        let (_, second, _) = call("/works", None).await;
        assert!(first.is_some() && second.is_some());
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn a_client_cannot_choose_it() {
        let (_, header, body) = call("/fails", Some("chosen-by-the-client")).await;
        let header = header.expect("a request id");
        assert_ne!(header, "chosen-by-the-client");
        assert!(uuid::Uuid::parse_str(&header).is_ok());
        assert_eq!(body["request_id"], header.as_str());
    }

    #[test]
    fn outside_a_request_there_is_none() {
        assert_eq!(current(), None);
    }
}
