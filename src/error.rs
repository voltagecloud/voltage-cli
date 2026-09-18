//! Failure categories, their documented exit codes, and the typed detail reported with them.

use crate::output::redact;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, Error>;

/// Documented failure class; the discriminant is the process exit code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    /// The API rejected the request or a payment reached an unsuccessful state.
    Api = 1,
    /// Invalid invocation or configuration.
    Usage = 2,
    /// Authentication or authorization failure.
    Auth = 3,
    /// Transport failure or a submission whose outcome is unknown.
    Transport = 4,
    /// A wait or pagination deadline passed.
    Timeout = 5,
    /// Interrupted by Ctrl-C.
    Interrupted = 130,
}

impl ErrorKind {
    pub fn exit_code(self) -> i32 {
        self as i32
    }

    /// Failure class for a non-success HTTP status from the Voltage API.
    pub fn for_http_status(status: u16) -> Self {
        if status == 401 || status == 403 {
            Self::Auth
        } else {
            Self::Api
        }
    }
}

/// State of a submission when its command could not confirm the result.
#[derive(Clone, Copy, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SubmissionOutcome {
    Unknown,
    Pending,
}

/// Structured context reported beside an error message.
///
/// Every variant serializes to the stable JSON object documented for its situation.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum ErrorDetail {
    /// The API answered with a non-success status.
    Http { http_status: u16, data: Value },
    /// A mutation was sent, but the transport failed before a result arrived.
    UncertainSubmission {
        resource_id: Option<Uuid>,
        organization_id: Option<Uuid>,
        environment_ids: Vec<Uuid>,
        outcome: SubmissionOutcome,
        action: &'static str,
    },
    /// A payment was still pending when its wait ended.
    Pending {
        resource_id: Uuid,
        outcome: SubmissionOutcome,
    },
    /// A payment's terminal state, as the API reported it.
    Payment(Value),
}

impl ErrorDetail {
    pub fn uncertain_submission(
        resource_id: Option<Uuid>,
        organization_id: Option<Uuid>,
        environment_ids: Vec<Uuid>,
    ) -> Self {
        Self::UncertainSubmission {
            resource_id,
            organization_id,
            environment_ids,
            outcome: SubmissionOutcome::Unknown,
            action: "Query the original resource ID before deciding whether to resubmit",
        }
    }

    pub fn pending(resource_id: Uuid) -> Self {
        Self::Pending {
            resource_id,
            outcome: SubmissionOutcome::Pending,
        }
    }

    /// API payloads embedded in a detail can echo credentials; scrub them before reporting.
    pub fn redact(&mut self, secrets: &[&str]) {
        match self {
            Self::Http { data, .. } | Self::Payment(data) => redact(data, secrets),
            Self::UncertainSubmission { .. } | Self::Pending { .. } => {}
        }
    }
}

#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    pub detail: Option<ErrorDetail>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: None,
        }
    }

    pub fn api(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Api, message)
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Auth, message)
    }

    pub fn transport(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Transport, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Timeout, message)
    }

    pub fn interrupted(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Interrupted, message)
    }

    pub fn with_detail(mut self, detail: ErrorDetail) -> Self {
        self.detail = Some(detail);
        self
    }

    /// The HTTP status behind an API failure.
    pub fn http_status(&self) -> Option<u16> {
        match self.detail {
            Some(ErrorDetail::Http { http_status, .. }) => Some(http_status),
            _ => None,
        }
    }

    pub fn is_transport(&self) -> bool {
        self.kind == ErrorKind::Transport
    }

    pub fn redacted(mut self, secrets: &[&str]) -> Self {
        if let Some(detail) = &mut self.detail {
            detail.redact(secrets);
        }
        self
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::transport(error.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::usage(format!("Invalid JSON: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exit_codes_match_the_documented_table() {
        assert_eq!(ErrorKind::Api.exit_code(), 1);
        assert_eq!(ErrorKind::Usage.exit_code(), 2);
        assert_eq!(ErrorKind::Auth.exit_code(), 3);
        assert_eq!(ErrorKind::Transport.exit_code(), 4);
        assert_eq!(ErrorKind::Timeout.exit_code(), 5);
        assert_eq!(ErrorKind::Interrupted.exit_code(), 130);
        assert_eq!(ErrorKind::for_http_status(403), ErrorKind::Auth);
        assert_eq!(ErrorKind::for_http_status(500), ErrorKind::Api);
    }

    #[test]
    fn details_keep_their_documented_shapes() {
        let id = Uuid::nil();
        assert_eq!(
            serde_json::to_value(ErrorDetail::pending(id)).unwrap(),
            json!({"resource_id": id, "outcome": "pending"})
        );
        assert_eq!(
            serde_json::to_value(ErrorDetail::uncertain_submission(Some(id), None, vec![id]))
                .unwrap(),
            json!({
                "resource_id": id,
                "organization_id": null,
                "environment_ids": [id],
                "outcome": "unknown",
                "action": "Query the original resource ID before deciding whether to resubmit"
            })
        );
        let http = Error::api("failed").with_detail(ErrorDetail::Http {
            http_status: 404,
            data: json!({"api_key": "secret"}),
        });
        assert_eq!(http.http_status(), Some(404));
        let redacted = serde_json::to_value(http.redacted(&[]).detail).unwrap();
        assert_eq!(
            redacted,
            json!({"http_status": 404, "data": {"api_key": "[REDACTED]"}})
        );
    }
}
