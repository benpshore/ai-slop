//! The one error shape every non-2xx response carries:
//!
//! ```json
//! {"error": {"code": "invalid_path", "message": "…", "details": [{"index": 1, "reason": "not_pdf"}]}}
//! ```
//!
//! `code` is stable within `/v1` and is what clients should branch on;
//! `message` is for people and may change. `details` is always present
//! (empty unless the error is about particular paths). No message contains
//! a file system path the client did not send (docs/API.md).

use std::borrow::Cow;

use http::StatusCode;
use serde::Serialize;

/// One rejected path in a batch: its position in `paths` and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct PathIssue {
    pub index: usize,
    pub reason: &'static str,
}

#[derive(Clone, Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: Cow<'static, str>,
    pub details: Vec<PathIssue>,
}

impl ApiError {
    pub fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: Vec::new(),
        }
    }

    pub fn bad_host() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "bad_host",
            "the Host header must name this server on loopback (127.0.0.1, [::1] or localhost, with its port)",
        )
    }

    pub fn bad_origin() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "bad_origin",
            "requests from other web origins are refused",
        )
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "send the token as `Authorization: Bearer <token>` (tpe-serve --print-token)",
        )
    }

    pub fn no_route() -> Self {
        Self::new(StatusCode::NOT_FOUND, "no_route", "no such endpoint")
    }

    pub fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "this endpoint does not accept that method",
        )
    }

    pub fn job_not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "no such job")
    }

    pub fn timeout() -> Self {
        Self::new(
            StatusCode::REQUEST_TIMEOUT,
            "request_timeout",
            "the request was not completed in time",
        )
    }

    pub fn internal(message: &'static str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// The JSON body.
    pub fn body(&self) -> Vec<u8> {
        #[derive(Serialize)]
        struct Inner<'a> {
            code: &'a str,
            message: &'a str,
            details: &'a [PathIssue],
        }
        #[derive(Serialize)]
        struct Outer<'a> {
            error: Inner<'a>,
        }
        serde_json::to_vec(&Outer {
            error: Inner {
                code: self.code,
                message: &self.message,
                details: &self.details,
            },
        })
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{ApiError, PathIssue};

    #[test]
    fn the_shape_is_stable() {
        let mut error = ApiError::no_route();
        assert_eq!(
            String::from_utf8(error.body()).unwrap(),
            r#"{"error":{"code":"no_route","message":"no such endpoint","details":[]}}"#
        );
        error.details.push(PathIssue {
            index: 2,
            reason: "not_pdf",
        });
        let value: serde_json::Value = serde_json::from_slice(&error.body()).unwrap();
        assert_eq!(value["error"]["details"][0]["index"], 2);
        assert_eq!(value["error"]["details"][0]["reason"], "not_pdf");
    }
}
