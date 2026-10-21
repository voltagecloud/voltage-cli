//! Failure categories, their documented exit codes, and the typed detail reported with them.

use crate::{
    config::Source,
    output::{redact, redact_text},
};
use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
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
    /// A change was described but not sent, because neither `--execute` nor
    /// `VOLTAGE_EXECUTE` asked for it to be sent.
    NotExecuted = 6,
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
    Http {
        http_status: u16,
        data: Value,
        /// Present when a 403 was answered to `VOLTAGE_API_KEY`.
        #[serde(skip_serializing_if = "Option::is_none")]
        credential_sources: Option<CredentialSources>,
    },
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
    /// The auth service rejected a saved credential.
    Credential {
        credential_problem: CredentialRejection,
    },
    /// A BOLT11 send without `--wallet`: the wallets in scope that could pay, and why the
    /// others cannot.
    WalletShortlist(crate::wallet_shortlist::Shortlist),
    /// `wallets list --all-orgs`: the organizations whose wallets could not be listed.
    FailedOrganizations {
        failed_organizations: Vec<crate::all_orgs::FailedOrganization>,
    },
}

/// Why the auth service rejected a saved credential. `voltage context` reads no secret and
/// makes no request, so it cannot report these; they appear only on a command's error.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialRejection {
    /// The auth service answered a saved login's refresh or token exchange with
    /// `invalid_grant`: the login expired, was revoked, or its grant was already spent.
    LoginExpired,
}

/// Where the credential and the organization of a request came from. A 403 reports it for
/// `VOLTAGE_API_KEY`, because that key takes precedence over a saved login without saying so.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CredentialSources {
    pub(crate) credential: Source,
    pub(crate) organization: Source,
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

    /// A detail the hint already gives in prose, which human output leaves out.
    pub fn repeats_hint(&self) -> bool {
        matches!(self, Self::WalletShortlist(_))
    }

    /// API payloads embedded in a detail can echo credentials; scrub them before reporting.
    pub fn redact(&mut self, secrets: &[&str]) {
        match self {
            Self::Http { data, .. } | Self::Payment(data) => redact(data, secrets),
            Self::UncertainSubmission { .. }
            | Self::Pending { .. }
            | Self::Credential { .. }
            | Self::WalletShortlist(_)
            | Self::FailedOrganizations { .. } => {}
        }
    }
}

#[derive(Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
    /// The next step to take, naming the command when there is one.
    pub hint: Option<Cow<'static, str>>,
    pub detail: Option<ErrorDetail>,
    pub(crate) stdout_closed: bool,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            hint: None,
            detail: None,
            stdout_closed: false,
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

    pub fn with_hint(mut self, hint: impl Into<Cow<'static, str>>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Add a sentence after the hint the error already has, so neither is lost.
    pub fn and_hint(mut self, hint: &'static str) -> Self {
        self.hint = Some(match self.hint.take() {
            Some(existing) => format!("{existing} {hint}").into(),
            None => hint.into(),
        });
        self
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

    /// Only a closed stdout is successful pipeline termination. A broken file, stdin, or
    /// stderr remains a transport failure.
    pub fn is_stdout_closed(&self) -> bool {
        self.stdout_closed
    }

    /// Remove the presented credentials from the message and detail; both can quote the API.
    pub fn redacted(mut self, secrets: &[&str]) -> Self {
        redact_text(&mut self.message, secrets);
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
        assert_eq!(ErrorKind::NotExecuted.exit_code(), 6);
        assert_eq!(ErrorKind::Interrupted.exit_code(), 130);
        assert_eq!(ErrorKind::for_http_status(403), ErrorKind::Auth);
        assert_eq!(ErrorKind::for_http_status(500), ErrorKind::Api);
    }

    #[test]
    fn and_hint_adds_a_hint_or_appends_to_the_existing_one() {
        let alone = Error::usage("failed").and_hint("Or pass --currency.");
        assert_eq!(alone.hint.as_deref(), Some("Or pass --currency."));
        let appended = Error::usage("failed")
            .with_hint("Sign in.")
            .and_hint("Or pass --currency.");
        assert_eq!(
            appended.hint.as_deref(),
            Some("Sign in. Or pass --currency.")
        );
    }

    #[test]
    fn io_errors_alone_do_not_indicate_closed_stdout() {
        let closed: Error = std::io::Error::from(std::io::ErrorKind::BrokenPipe).into();
        assert!(!closed.is_stdout_closed());
        assert_eq!(closed.kind, ErrorKind::Transport);
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
        assert_eq!(
            serde_json::to_value(ErrorDetail::Credential {
                credential_problem: CredentialRejection::LoginExpired
            })
            .unwrap(),
            json!({"credential_problem": "login_expired"})
        );
        let http = Error::api("failed").with_detail(ErrorDetail::Http {
            http_status: 404,
            data: json!({"api_key": "secret"}),
            credential_sources: None,
        });
        assert_eq!(http.http_status(), Some(404));
        let redacted = serde_json::to_value(http.redacted(&[]).detail).unwrap();
        assert_eq!(
            redacted,
            json!({"http_status": 404, "data": {"api_key": "[REDACTED]"}})
        );
        let forbidden = ErrorDetail::Http {
            http_status: 403,
            data: Value::Null,
            credential_sources: Some(CredentialSources {
                credential: Source::Environment,
                organization: Source::Flag,
            }),
        };
        assert_eq!(
            serde_json::to_value(forbidden).unwrap(),
            json!({
                "http_status": 403,
                "data": null,
                "credential_sources": {"credential": "environment", "organization": "flag"}
            })
        );
        let echoed = Error::api("HTTP 400: Bad key key-1").redacted(&["key-1", ""]);
        assert_eq!(echoed.message, "HTTP 400: Bad key [REDACTED]");
    }
}
