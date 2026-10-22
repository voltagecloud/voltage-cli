//! Failure categories, their documented exit codes, and the typed detail reported with them.

use crate::{
    config::Source,
    output::{redact, redact_text},
};
use serde::Serialize;
use serde_json::Value;
use std::{borrow::Cow, error::Error as StdError, io, time::Duration};
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

/// How long a request had to get a response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseLimit {
    /// `--timeout`, which a larger value raises.
    Timeout,
    /// A fixed limit that `--timeout` does not change, such as the auth service's.
    Fixed(Duration),
}

/// Why a request got no HTTP response. Each class has a different fix, so the message
/// names it; the underlying error's text can echo hosts and paths and is never shown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransportFailure {
    /// No response within the request's limit.
    Timeout(ResponseLimit),
    /// The connection was not established within the fixed connect limit, which
    /// `--timeout` does not change: an unreachable host or a firewall that drops packets.
    ConnectTimeout,
    /// The TLS handshake failed, for example because the certificate was not trusted.
    Tls,
    /// The connection was not established: DNS, a refused port, or a proxy.
    Connect,
    /// The connection broke before a response arrived.
    Other,
}

impl TransportFailure {
    fn of(error: &reqwest::Error, limit: ResponseLimit) -> Self {
        let mut tls = false;
        let mut cause = error.source();
        while let Some(current) = cause {
            tls |= is_tls(current);
            cause = current.source();
        }
        Self::classify(error.is_timeout(), error.is_connect(), tls, limit)
    }

    /// A connect timeout is both a timeout and a connect error. It is checked first, because
    /// it ends at the connect limit, not at `--timeout`.
    fn classify(timeout: bool, connect: bool, tls: bool, limit: ResponseLimit) -> Self {
        match (timeout, connect, tls) {
            (true, true, _) => Self::ConnectTimeout,
            (true, false, _) => Self::Timeout(limit),
            (false, _, true) => Self::Tls,
            (false, true, false) => Self::Connect,
            (false, false, false) => Self::Other,
        }
    }

    fn what(self) -> Cow<'static, str> {
        match self {
            Self::Timeout(ResponseLimit::Timeout) => "no response within --timeout".into(),
            Self::Timeout(ResponseLimit::Fixed(limit)) => {
                format!("no response within {} seconds", limit.as_secs()).into()
            }
            Self::ConnectTimeout => "could not connect within 10 seconds".into(),
            Self::Tls => {
                "the TLS connection failed, for example because the server's certificate was not trusted".into()
            }
            Self::Connect => "could not connect".into(),
            Self::Other => "the connection closed before a response arrived".into(),
        }
    }

    /// The next step. A `write` that may have reached the server must not be repeated blindly:
    /// a rerun can apply the change again, so the resource is looked up first. A failed
    /// connection or TLS handshake sent nothing, so its fix is the same for reads and writes.
    fn next_step(self, service: &str, write: bool) -> String {
        const LOOK_UP_FIRST: &str = "The change may have been applied. Look up the resource ID before you run the command again, because a rerun can repeat the change.";
        match self {
            Self::Timeout(ResponseLimit::Timeout) if write => {
                format!(
                    "{LOOK_UP_FIRST} A larger --timeout gives the {service} service longer to answer."
                )
            }
            Self::Timeout(ResponseLimit::Fixed(limit)) if write => format!(
                "{LOOK_UP_FIRST} The {service} service has a fixed {}-second limit, which --timeout does not change.",
                limit.as_secs()
            ),
            Self::Other if write => format!(
                "{LOOK_UP_FIRST} Then check the {service} URL and whether the {service} service is running."
            ),
            Self::Timeout(ResponseLimit::Timeout) => "Retry, or pass a larger --timeout.".into(),
            Self::Timeout(ResponseLimit::Fixed(limit)) => format!(
                "Retry. The {service} service has a fixed {}-second limit, which --timeout does not change.",
                limit.as_secs()
            ),
            Self::ConnectTimeout => format!(
                "Check the {service} URL's host, your network or firewall, and whether the {service} service is reachable. --timeout does not change this connect limit."
            ),
            Self::Tls => format!(
                "Check the {service} URL's host, and that this system trusts its certificate, including any proxy that inspects TLS."
            ),
            Self::Connect => format!(
                "Check the {service} URL, your network or proxy, and whether the {service} service is running."
            ),
            Self::Other => {
                format!("Check the {service} URL and whether the {service} service is running.")
            }
        }
    }
}

/// Whether `error` is a rustls error. rustls errors reach reqwest inside nested `io::Error`s,
/// whose `source()` skips the error they wrap, so each `io::Error` is unwrapped explicitly.
fn is_tls(error: &(dyn StdError + 'static)) -> bool {
    if error.is::<rustls::Error>() {
        return true;
    }
    error
        .downcast_ref::<io::Error>()
        .and_then(io::Error::get_ref)
        .is_some_and(|inner| is_tls(inner))
}

impl Error {
    /// A request to `service` that got no response within `limit`. `request` names it, the
    /// message says why it failed, and `consequence` adds what the caller must know, such as
    /// a possible write; the hint names the next step, which for a `write` never says to retry.
    pub fn no_response(
        error: &reqwest::Error,
        request: &str,
        consequence: &str,
        service: &str,
        limit: ResponseLimit,
        write: bool,
    ) -> Self {
        let failure = TransportFailure::of(error, limit);
        Self::transport(format!("{request} failed: {}{consequence}", failure.what()))
            .with_hint(failure.next_step(service, write))
    }

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
    use crate::auth::client;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
        time::Duration,
    };

    /// Accept one connection on a loopback port and hand it to `serve` on a thread.
    fn serve_once(serve: impl FnOnce(TcpStream) + Send + 'static) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                serve(stream);
            }
        });
        port
    }

    fn read_request(stream: &mut TcpStream) {
        let mut buffer = [0; 1024];
        let _ = stream.read(&mut buffer);
    }

    async fn failure(url: String, timeout: Duration) -> TransportFailure {
        let error = client(timeout).unwrap().get(url).send().await.unwrap_err();
        TransportFailure::of(&error, ResponseLimit::Timeout)
    }

    #[tokio::test]
    async fn transport_failures_are_classified_by_their_fix() {
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let refused = failure(
            format!("http://127.0.0.1:{closed}/"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(refused, TransportFailure::Connect);

        // A server that answers TLS with plain HTTP fails the handshake.
        let plain = serve_once(|mut stream| {
            read_request(&mut stream);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
        });
        let tls = failure(
            format!("https://127.0.0.1:{plain}/"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(tls, TransportFailure::Tls);

        // The silent server holds the connection until its test ends.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let silent = serve_once(move |stream| {
            let _ = held.recv();
            drop(stream);
        });
        let timeout = failure(
            format!("http://127.0.0.1:{silent}/"),
            Duration::from_millis(100),
        )
        .await;
        assert_eq!(timeout, TransportFailure::Timeout(ResponseLimit::Timeout));
        drop(release);

        let closing = serve_once(|mut stream| read_request(&mut stream));
        let closed_early = failure(
            format!("http://127.0.0.1:{closing}/"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(closed_early, TransportFailure::Other);
    }

    #[tokio::test]
    async fn no_response_names_the_failure_and_the_next_step_for_its_service() {
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = client(Duration::from_secs(5))
            .unwrap()
            .get(format!("http://127.0.0.1:{closed}/"))
            .send()
            .await
            .unwrap_err();
        let reported = Error::no_response(
            &error,
            "Price request",
            "; nothing was retried",
            "price",
            ResponseLimit::Timeout,
            false,
        );
        assert_eq!(reported.kind, ErrorKind::Transport);
        assert_eq!(
            reported.message,
            "Price request failed: could not connect; nothing was retried"
        );
        assert_eq!(
            reported.hint.as_deref(),
            Some(
                "Check the price URL, your network or proxy, and whether the price service is running."
            )
        );
        // Raw transport text, which names the host and port, stays out of the message.
        assert!(!reported.message.contains(&closed.to_string()));
    }

    #[test]
    fn each_failure_has_its_own_next_step() {
        assert_eq!(
            TransportFailure::Timeout(ResponseLimit::Timeout).next_step("API", false),
            "Retry, or pass a larger --timeout."
        );
        assert!(
            TransportFailure::Tls
                .next_step("API", false)
                .contains("trusts its certificate")
        );
        assert_eq!(
            TransportFailure::Other.next_step("auth", false),
            "Check the auth URL and whether the auth service is running."
        );
        assert_ne!(TransportFailure::Tls.what(), TransportFailure::Other.what());
        let connect = TransportFailure::ConnectTimeout;
        assert_eq!(connect.what(), "could not connect within 10 seconds");
        assert_eq!(crate::auth::CONNECT_TIMEOUT, Duration::from_secs(10));
        assert!(
            connect
                .next_step("API", false)
                .contains("--timeout does not change")
        );
    }

    #[test]
    fn a_fixed_limit_is_named_and_never_blamed_on_timeout_flag() {
        let fixed = TransportFailure::Timeout(ResponseLimit::Fixed(Duration::from_secs(30)));
        assert_eq!(fixed.what(), "no response within 30 seconds");
        assert_eq!(
            fixed.next_step("auth", false),
            "Retry. The auth service has a fixed 30-second limit, which --timeout does not change."
        );
        assert!(fixed.next_step("auth", true).ends_with(
            "The auth service has a fixed 30-second limit, which --timeout does not change."
        ));
        let flag = TransportFailure::Timeout(ResponseLimit::Timeout);
        assert_eq!(flag.what(), "no response within --timeout");
        for write in [false, true] {
            assert!(!fixed.next_step("auth", write).contains("larger --timeout"));
            assert!(flag.next_step("API", write).contains("larger --timeout"));
        }
    }

    #[test]
    fn a_write_that_may_have_been_applied_is_never_told_to_retry() {
        for failure in [
            TransportFailure::Timeout(ResponseLimit::Timeout),
            TransportFailure::Timeout(ResponseLimit::Fixed(Duration::from_secs(30))),
            TransportFailure::Other,
        ] {
            let step = failure.next_step("API", true);
            assert!(step.starts_with("The change may have been applied. Look up the resource ID before you run the command again"), "{step}");
            assert!(!step.contains("Retry"), "{step}");
        }
        // Nothing was sent when the connection or handshake failed, so the fix is the read's.
        for failure in [
            TransportFailure::Connect,
            TransportFailure::ConnectTimeout,
            TransportFailure::Tls,
        ] {
            assert_eq!(
                failure.next_step("API", true),
                failure.next_step("API", false)
            );
        }
    }

    #[test]
    fn a_connect_timeout_is_classified_before_a_timeout() {
        use TransportFailure as F;
        let limit = ResponseLimit::Timeout;
        for (timeout, connect, tls, expected) in [
            (true, true, false, F::ConnectTimeout),
            (true, true, true, F::ConnectTimeout),
            (true, false, false, F::Timeout(limit)),
            (false, true, true, F::Tls),
            (false, false, true, F::Tls),
            (false, true, false, F::Connect),
            (false, false, false, F::Other),
        ] {
            assert_eq!(
                F::classify(timeout, connect, tls, limit),
                expected,
                "{timeout} {connect} {tls}"
            );
        }
    }

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
