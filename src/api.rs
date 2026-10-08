//! HTTP execution against the Voltage API: submission, waiting, pagination, and streaming.
//!
//! A mutation is sent exactly once. When its transport fails, the CLI reconciles by reading
//! the original resource ID within a short bound; a missing projection stays uncertain and is
//! reported with the ID so the operator can decide. Every wait, poll, and page loop ends at
//! the command deadline.

use crate::{
    Error, Result,
    auth::{self, ApiCredential},
    backoff::{Backoff, POLL_CEILING},
    cli::{ApiInvocation, GlobalFlags, Origin, Requested},
    config::{
        API_URL, EXECUTE_VARIABLE, Scope, Settings, new_private, private_dir, read_private,
        read_secret,
    },
    dry_run,
    error::{ErrorDetail, ErrorKind},
    input::{PaginationMode, Request},
    output::{Envelope, Outcome, Output},
    payment::{PaymentDirection, PaymentView, ReceiveKind, StatusText, WaitProgress, WaitTarget},
    registry::{AuthScheme, Method, Operation, OperationId},
    secret::Secret,
    terminal::Terminal,
};
use futures_util::StreamExt;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::Write,
    sync::{Mutex, OnceLock, PoisonError},
    time::Duration,
};
use tokio::time::Instant;
use uuid::Uuid;
use zeroize::Zeroizing;

/// API responses are bounded; list pages and projections are far smaller than this.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// One server-sent event, and one line of one, may not exceed this.
const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// A receive's invoice is usually ready within this long.
const READY_POLL_START: Duration = Duration::from_millis(100);
/// A send usually settles within this long.
const COMPLETION_POLL_START: Duration = Duration::from_millis(250);
/// A checkout session projection usually appears within this long.
const SESSION_POLL_START: Duration = Duration::from_millis(100);
/// Waits repeat their status notice at least this often.
const STATUS_NOTICE_INTERVAL: Duration = Duration::from_secs(15);
/// The one read that reconciles an interrupted payment submission.
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(5);
const JOURNAL_DIR: &str = "requests";

/// Credential the client presents, decided once from the operation's auth scheme.
enum Authorization {
    Account(ApiCredential),
    CheckoutSession(Secret),
    CheckoutStream(Secret),
    None,
}

impl Authorization {
    /// Checkout credentials come from `--token-file` or their environment variable and never
    /// fall back to the account credential.
    async fn resolve(
        invocation: &ApiInvocation,
        global: &GlobalFlags,
        settings: &Settings,
        scope: &Scope,
        submission: &SubmissionState,
    ) -> Result<Self> {
        let scheme = invocation.operation.auth;
        Ok(match scheme {
            AuthScheme::Account => Self::Account(
                auth::resolve_organization(settings, scope, global, submission).await?,
            ),
            AuthScheme::CheckoutSession => {
                Self::CheckoutSession(checkout_token(invocation, "VOLTAGE_CHECKOUT_TOKEN").await?)
            }
            AuthScheme::CheckoutStream => {
                Self::CheckoutStream(checkout_token(invocation, "VOLTAGE_STREAM_TOKEN").await?)
            }
            AuthScheme::None => Self::None,
        })
    }

    /// Text that must never appear in output.
    fn secrets(&self) -> Vec<&str> {
        match self {
            Self::Account(ApiCredential::ApiKey(secret))
            | Self::Account(ApiCredential::OrganizationToken(secret))
            | Self::CheckoutSession(secret)
            | Self::CheckoutStream(secret) => vec![secret.expose()],
            Self::None => Vec::new(),
        }
    }
}

/// A checkout credential from `--token-file`, else from `variable`.
async fn checkout_token(invocation: &ApiInvocation, variable: &str) -> Result<Secret> {
    if let Some(source) = &invocation.token_file {
        return read_secret(source).await;
    }
    std::env::var(variable)
        .ok()
        .map(Secret::new)
        .filter(|secret| !secret.expose().trim().is_empty())
        .ok_or_else(|| Error::auth(format!("Supply --token-file or {variable}")))
}

struct Response {
    status: u16,
    body: Value,
    /// Server pacing for the next poll, from `x-retry-after-ms` or `retry-after`.
    retry_after: Option<Duration>,
}

struct Api {
    client: reqwest::Client,
    base: String,
    authorization: Authorization,
    origin: Option<Origin>,
    terminal: Terminal,
}

impl Api {
    fn new(
        global: &GlobalFlags,
        invocation: &ApiInvocation,
        authorization: Authorization,
    ) -> Result<Self> {
        Ok(Self {
            client: auth::client(global.timeout)?,
            base: auth::base_url(global.api_url.as_deref().unwrap_or(API_URL))?,
            authorization,
            origin: invocation.origin.clone(),
            terminal: global.terminal(),
        })
    }

    fn secrets(&self) -> Vec<&str> {
        self.authorization.secrets()
    }

    fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .query(query);
        request = match &self.authorization {
            Authorization::Account(ApiCredential::ApiKey(key)) => {
                request.header("x-api-key", key.expose())
            }
            Authorization::Account(ApiCredential::OrganizationToken(token))
            | Authorization::CheckoutSession(token) => request.bearer_auth(token.expose()),
            Authorization::CheckoutStream(token) => {
                request.query(&[("stream_token", token.expose())])
            }
            Authorization::None => request,
        };
        if let Some(origin) = &self.origin {
            request = request.header("Origin", origin.as_str());
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        request
    }

    /// Send one request and parse its JSON. A failed read can be repeated; a failed write
    /// may have been submitted, and the messages say so.
    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<Response> {
        let read_only = method.is_read_only();
        let _progress = self.terminal.progress(if read_only {
            "Waiting for API response..."
        } else {
            "Submitting request..."
        });
        let response = self
            .request(method.into(), path, query, body)
            .send()
            .await
            .map_err(|_| {
                Error::transport(if read_only {
                    "HTTP read request failed. Check the API URL and whether the API service is running."
                } else {
                    "HTTP request failed; a write may have been submitted. No mutation was retried."
                })
            })?;
        let status = response.status().as_u16();
        let retry_after = retry_after(response.headers());
        let bytes = bounded_body(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|failure| match failure {
                BodyFailure::TooLarge => Error::transport("Response exceeds 64 MiB"),
                BodyFailure::Interrupted if read_only => {
                    Error::transport("HTTP read response was interrupted; retry this read.")
                }
                BodyFailure::Interrupted => Error::transport(
                    "Response was interrupted; reconcile writes by their original ID",
                ),
            })?;
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).map_err(|_| {
                let kind = if (200..300).contains(&status) {
                    ErrorKind::Transport
                } else {
                    ErrorKind::for_http_status(status)
                };
                Error::new(kind, format!("HTTP {status}: non-JSON response omitted"))
            })?
        };
        if !(200..300).contains(&status) {
            return Err(Error::new(
                ErrorKind::for_http_status(status),
                rejection_message(status, &body),
            )
            .with_detail(ErrorDetail::Http {
                http_status: status,
                data: body,
            })
            .redacted(&self.secrets()));
        }
        Ok(Response {
            status,
            body,
            retry_after,
        })
    }

    async fn read(&self, path: &str) -> Result<Response> {
        self.send(Method::Get, path, &[], None).await
    }

    /// Follow a checkout event stream, writing one envelope per event until it closes.
    async fn stream(&self, request: &Request, out: &mut Output) -> Result<()> {
        let response = self
            .terminal
            .during(
                "Opening checkout event stream...",
                self.request(reqwest::Method::GET, &request.path, &request.query, None)
                    .send(),
            )
            .await
            .map_err(|_| Error::transport("Could not open checkout event stream"))?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(Error::new(
                ErrorKind::for_http_status(status),
                format!("Event stream returned HTTP {status}"),
            ));
        }
        if !response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            return Err(Error::transport("Expected text/event-stream"));
        }
        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut assembler = EventAssembler::default();
        loop {
            let chunk = self
                .terminal
                .during("Waiting for checkout event...", stream.next())
                .await;
            let Some(chunk) = chunk else { break };
            let chunk = chunk.map_err(|_| Error::transport("Checkout stream disconnected"))?;
            pending.extend_from_slice(&chunk);
            if pending.len() > MAX_EVENT_BYTES {
                return Err(Error::transport("Event stream line exceeds 1 MiB"));
            }
            while let Some(end) = pending.iter().position(|byte| *byte == b'\n') {
                let raw: Vec<u8> = pending.drain(..=end).collect();
                let line = std::str::from_utf8(&raw)
                    .map_err(|_| Error::transport("Invalid UTF-8 in event stream"))?
                    .trim_end_matches(['\r', '\n']);
                if let Some(envelope) = assembler.line(line)? {
                    out.write(envelope, &self.secrets())?;
                }
            }
        }
        Ok(())
    }
}

pub(crate) enum BodyFailure {
    TooLarge,
    Interrupted,
}

/// Error types the CLI explains specially; the API's other types are shown as detail.
#[derive(Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ApiErrorType {
    FeatureFlagDisabled,
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, Deserialize)]
struct ApiErrorBody {
    #[serde(default, rename = "type")]
    kind: ApiErrorType,
    #[serde(default)]
    detail: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ApiErrorView {
    #[serde(default)]
    error: ApiErrorBody,
}

/// The message for a rejected request; a disabled organization feature names the fix.
fn rejection_message(status: u16, body: &Value) -> String {
    let rejection = ApiErrorView::deserialize(body).unwrap_or_default();
    match rejection.error.kind {
        ApiErrorType::FeatureFlagDisabled => format!(
            "Voltage API returned HTTP {status}: {}. Ask Voltage to enable this feature for the organization.",
            rejection
                .error
                .detail
                .as_deref()
                .unwrap_or("this feature is not enabled for the organization")
        ),
        ApiErrorType::Other => format!("Voltage API returned HTTP {status}"),
    }
}

/// Read a response body chunk by chunk into zeroized memory, refusing to grow past `limit`.
/// Responses can carry one-time secrets on their way to a private file or redacted output.
pub(crate) async fn bounded_body(
    mut response: reqwest::Response,
    limit: usize,
) -> std::result::Result<Zeroizing<Vec<u8>>, BodyFailure> {
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| BodyFailure::Interrupted)?
    {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(BodyFailure::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A server hint never waits longer than this, so a hostile header cannot overflow an
/// `Instant` or stall a poll forever.
const MAX_RETRY_HINT: Duration = Duration::from_secs(300);

/// `x-retry-after-ms` wins; `retry-after` may be seconds or an HTTP date.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    if let Some(millis) = text("x-retry-after-ms").and_then(|value| value.parse::<u64>().ok()) {
        return Some(Duration::from_millis(millis).min(MAX_RETRY_HINT));
    }
    let value = text("retry-after")?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds).min(MAX_RETRY_HINT));
    }
    httpdate::parse_http_date(value).ok().map(|at| {
        at.duration_since(std::time::SystemTime::now())
            .unwrap_or_default()
            .min(MAX_RETRY_HINT)
    })
}

/// Server-sent event fields the CLI understands; other fields are ignored per the spec.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventField {
    Event,
    Data,
    Id,
}

impl EventField {
    /// Split a line into its field and value, dropping the optional space after the colon.
    fn parse(line: &str) -> Option<(Self, &str)> {
        let (name, value) = line.split_once(':')?;
        let field = match name {
            "event" => Self::Event,
            "data" => Self::Data,
            "id" => Self::Id,
            _ => return None,
        };
        Some((field, value.strip_prefix(' ').unwrap_or(value)))
    }
}

/// Assembles one server-sent event from its lines; a blank line dispatches it.
#[derive(Default)]
struct EventAssembler {
    event: String,
    data: Vec<String>,
    id: Option<String>,
}

impl EventAssembler {
    fn line(&mut self, line: &str) -> Result<Option<Envelope>> {
        if line.is_empty() {
            let envelope = (!self.data.is_empty()).then(|| {
                let joined = self.data.join("\n");
                let value = serde_json::from_str(&joined).unwrap_or(Value::String(joined));
                let event = if self.event.is_empty() {
                    "message".to_owned()
                } else {
                    self.event.clone()
                };
                self.data.clear();
                Envelope::event(value, event, self.id.clone())
            });
            self.event.clear();
            return Ok(envelope);
        }
        match EventField::parse(line) {
            Some((EventField::Event, value)) => self.event = value.to_owned(),
            Some((EventField::Data, value)) => {
                self.data.push(value.to_owned());
                if self.data.iter().map(String::len).sum::<usize>() > MAX_EVENT_BYTES {
                    return Err(Error::transport("Event exceeds 1 MiB"));
                }
            }
            Some((EventField::Id, value)) => self.id = Some(value.to_owned()),
            None => {}
        }
        Ok(None)
    }
}

/// Run one registry operation end to end and write its envelopes.
pub async fn execute(
    invocation: &ApiInvocation,
    execution: Execution,
    global: &GlobalFlags,
    settings: &Settings,
    scope: &Scope,
    out: &mut Output,
    submission: &SubmissionState,
) -> Result<()> {
    let operation = invocation.operation;
    let mut request = Request::read(invocation, scope).await?;
    check_invoice_flags(invocation, &request)?;
    let run = Run::plan(invocation, &request, scope);
    match execution {
        Execution::DryRun => {
            global
                .terminal()
                .notice("Dry run: nothing was authenticated, confirmed, recorded, or sent.");
            return out.write(dry_run::describe(invocation, global, &request, run)?, &[]);
        }
        Execution::DescribeChange => {
            out.write(dry_run::describe(invocation, global, &request, run)?, &[])?;
            return Err(Error::new(
                ErrorKind::NotExecuted,
                format!(
                    "Described this change without sending it; pass --execute or set {EXECUTE_VARIABLE}=1 to send it"
                ),
            ));
        }
        Execution::SendFromEnvironment => global.terminal().notice(format!(
            "Sending this change because {EXECUTE_VARIABLE} is set."
        )),
        Execution::Send => {}
    }
    if operation.id.returns_one_time_secret() && !out.secure_destination() {
        return Err(Error::usage(
            "This operation returns a one-time secret; supply --output-file PATH or --show-secrets before executing",
        ));
    }
    let authorization =
        Authorization::resolve(invocation, global, settings, scope, submission).await?;
    let api = Api::new(global, invocation, authorization)?;
    let steps = Steps {
        api: &api,
        invocation,
        scope,
        deadline: Instant::now() + global.timeout,
    };
    if request.pagination_mode() == PaginationMode::Offset && operation.has_parameter("cursor") {
        api.terminal.important(OFFSET_PAGINATION_DEPRECATED);
    }
    match run {
        Run::EventStream => api.stream(&request, out).await,
        Run::Read(read) => read.execute(&steps, &mut request, out).await,
        Run::Mutation(mutation) => {
            mutation
                .execute(&steps, &mut request, settings, submission, out)
                .await
        }
    }
}

/// Whether an API command sends its request, and what decided it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Execution {
    /// `--dry-run`: describe the command and succeed.
    DryRun,
    /// A change with neither `--execute` nor `VOLTAGE_EXECUTE`: describe it, then fail, so
    /// a script that forgot `--execute` does not pass silently.
    DescribeChange,
    /// A read, or a change sent because of `--execute`.
    Send,
    /// A change sent because `VOLTAGE_EXECUTE` is set; the command says so.
    SendFromEnvironment,
}

impl Execution {
    /// The flag decides first, then `VOLTAGE_EXECUTE`, then the default: reads are sent and
    /// changes are described.
    pub fn resolve(invocation: &ApiInvocation, execute_variable: Option<bool>) -> Self {
        match invocation.requested {
            Requested::DryRun => Self::DryRun,
            Requested::Execute => Self::Send,
            Requested::Unspecified if invocation.operation.method == Method::Get => Self::Send,
            Requested::Unspecified if execute_variable == Some(true) => Self::SendFromEnvironment,
            Requested::Unspecified => Self::DescribeChange,
        }
    }

    /// The command describes its request instead of sending it.
    pub fn describes(self) -> bool {
        matches!(self, Self::DryRun | Self::DescribeChange)
    }
}

pub const OFFSET_PAGINATION_DEPRECATED: &str =
    "Offset pagination is deprecated by the API; omit --offset and --pagination to page by cursor.";

/// What a validated API command does, decided once after local validation. `execute`
/// follows it and `--dry-run` reports it, so the two cannot disagree.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Run {
    /// A GET: send it, check the response, then finish as `then` says.
    Read(Read),
    /// Any other method: guard it, record it, send it once, then write or wait.
    Mutation(Mutation),
    /// A checkout event stream: open it with the stream token and follow it.
    EventStream,
}

impl Run {
    pub fn plan(invocation: &ApiInvocation, request: &Request, scope: &Scope) -> Self {
        let operation = invocation.operation;
        if operation.auth == AuthScheme::CheckoutStream {
            Self::EventStream
        } else if operation.method == Method::Get {
            Self::Read(Read::plan(invocation, scope))
        } else {
            Self::Mutation(Mutation::plan(invocation, request, scope))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Read {
    /// `wallets get` checks that the returned wallet is in the selected environment.
    pub check_wallet_environment: bool,
    pub then: ReadThen,
}

/// How a read finishes after its first response.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadThen {
    /// Write the response, then follow further pages when `follow` (`--all`) is set.
    Pages { follow: bool },
    /// A checkout session read answers 202 until its projection exists.
    SessionProjection,
    /// Poll the payment until `until`. A submitted payment can be missing from the read
    /// projection at first; only an explicit `--wait` treats that 404 as pending, while
    /// `--qr` and `--copy` alone fail like an ordinary read.
    PaymentWait {
        until: WaitTarget,
        missing_is_pending: bool,
    },
}

impl Read {
    fn plan(invocation: &ApiInvocation, scope: &Scope) -> Self {
        let operation = invocation.operation;
        let then = match wait_target(invocation) {
            Some(until) => ReadThen::PaymentWait {
                until,
                missing_is_pending: operation.id == OperationId::GetPayment
                    && invocation.wait.is_some(),
            },
            None if operation.id == OperationId::GetSession => ReadThen::SessionProjection,
            None => ReadThen::Pages {
                follow: invocation.all,
            },
        };
        Self {
            check_wallet_environment: operation.id == OperationId::GetWallet
                && !scope.envs.is_empty(),
            then,
        }
    }

    async fn execute(
        self,
        steps: &Steps<'_>,
        request: &mut Request,
        out: &mut Output,
    ) -> Result<()> {
        let operation = steps.invocation.operation;
        let sent = steps
            .api
            .send(
                operation.method,
                &request.path,
                &request.query,
                request.body.as_ref(),
            )
            .await;
        let response = match (sent, self.then) {
            (
                Err(error),
                ReadThen::PaymentWait {
                    missing_is_pending: true,
                    ..
                },
            ) if error.http_status() == Some(404) => Response {
                status: 404,
                body: Value::Null,
                retry_after: None,
            },
            (sent, _) => sent?,
        };
        if self.check_wallet_environment {
            require_wallet_in_environment(&response.body, steps.scope)?;
        }
        check_invoice_payment(steps.invocation, &response)?;
        match self.then {
            ReadThen::PaymentWait { until, .. } => {
                wait_for_payment(steps, request, response, until, out).await
            }
            ReadThen::SessionProjection => {
                let response =
                    await_session_projection(steps.api, request, response, steps.deadline).await?;
                write_pages(steps, request, response, Outcome::Retrieved, false, out).await
            }
            ReadThen::Pages { follow } => {
                write_pages(steps, request, response, Outcome::Retrieved, follow, out).await
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Mutation {
    /// A wallet mutation with an environment scope first reads the wallet to verify it,
    /// because wallets have organization scope.
    pub verify_wallet_environment: bool,
    /// The body ID recorded in the recovery journal before submission.
    pub journal_id: Option<Uuid>,
    /// A payment whose submission fails in transport is looked up by its ID.
    pub reconcile: bool,
    pub wait: Option<WaitTarget>,
}

impl Mutation {
    fn plan(invocation: &ApiInvocation, request: &Request, scope: &Scope) -> Self {
        let operation = invocation.operation;
        Self {
            verify_wallet_environment: operation.targets_wallet() && !scope.envs.is_empty(),
            // `body_id` comes from the body, so only a mutation with a body is journaled.
            journal_id: request.body_id,
            reconcile: operation.id.submits_payment(),
            wait: wait_target(invocation),
        }
    }

    /// Verify and journal, then send the mutation exactly once. A transport
    /// failure is reported as an uncertain submission unless reconciliation proves acceptance.
    async fn execute(
        self,
        steps: &Steps<'_>,
        request: &mut Request,
        settings: &Settings,
        submission: &SubmissionState,
        out: &mut Output,
    ) -> Result<()> {
        let operation = steps.invocation.operation;
        let scope = steps.scope;
        if self.verify_wallet_environment {
            verify_wallet_environment(steps.api, scope, steps.invocation.resource_id).await?;
        }
        // `--execute` is the approval, so nothing asks again; a send still states that its
        // fee limit is not the whole cost.
        if operation.id == OperationId::CreatePayment && request.is_consequential(operation) {
            steps.api.terminal.important(PAYMENT_FEE_LIMIT_NOTE);
        }
        if let (Some(id), Some(body)) = (self.journal_id, &request.body) {
            journal(settings, operation, scope, body, id, steps.api.terminal)?;
        }
        submission.record(operation.id, request.resource_id, scope);
        let sent = steps
            .api
            .send(
                operation.method,
                &request.path,
                &request.query,
                request.body.as_ref(),
            )
            .await;
        let (response, reconciled) = match sent {
            Ok(response) => (response, false),
            Err(error) if self.reconcile && error.is_transport() => {
                match reconcile_payment(steps.api, request, scope).await {
                    Some(response) => (response, true),
                    None => return Err(uncertain(error, request, scope)),
                }
            }
            Err(error) => return Err(uncertain(error, request, scope)),
        };
        check_invoice_payment(steps.invocation, &response)?;
        match self.wait {
            Some(until) => wait_for_payment(steps, request, response, until, out).await,
            None => {
                let settled = if reconciled {
                    Outcome::Accepted
                } else {
                    Outcome::Succeeded
                };
                write_pages(steps, request, response, settled, false, out).await
            }
        }
    }
}

/// What every step of one command's run shares.
#[derive(Clone, Copy)]
struct Steps<'a> {
    api: &'a Api,
    invocation: &'a ApiInvocation,
    scope: &'a Scope,
    deadline: Instant,
}

/// A transport failure after a mutation may have been sent leaves its outcome unknown.
fn uncertain(mut error: Error, request: &Request, scope: &Scope) -> Error {
    if error.is_transport() {
        error.detail = Some(ErrorDetail::uncertain_submission(
            request.resource_id,
            scope.org,
            scope.envs.clone(),
        ));
    }
    error
}

/// `--qr` and `--copy` imply `--wait ready` when no explicit wait was given.
fn wait_target(invocation: &ApiInvocation) -> Option<WaitTarget> {
    invocation
        .wait
        .or_else(|| invocation.presents_invoice().then_some(WaitTarget::Ready))
}

/// An invoice can only be presented for a receive.
fn check_invoice_payment(invocation: &ApiInvocation, response: &Response) -> Result<()> {
    if invocation.presents_invoice()
        && PaymentView::from_body(&response.body).direction == Some(PaymentDirection::Send)
    {
        return Err(Error::usage(
            "--qr and --copy require a BOLT11 receive payment",
        ));
    }
    Ok(())
}

/// `--qr` and `--copy` apply to BOLT11 receives only: `payments receive --kind bolt11` or a
/// `payments get` whose projection turns out to be a receive.
fn check_invoice_flags(invocation: &ApiInvocation, request: &Request) -> Result<()> {
    if !invocation.presents_invoice() {
        return Ok(());
    }
    let receive_alias = invocation.alias == Some(PaymentDirection::Receive);
    if !receive_alias && invocation.operation.id != OperationId::GetPayment {
        return Err(Error::usage(
            "--qr and --copy are only available for payments receive and payments get",
        ));
    }
    if receive_alias && request.receive_kind() != Some(ReceiveKind::Bolt11) {
        return Err(Error::usage(
            "--qr and --copy require a BOLT11 receive (--kind bolt11)",
        ));
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
struct WalletView {
    #[serde(default)]
    environment_id: Option<Uuid>,
}

fn require_wallet_in_environment(body: &Value, scope: &Scope) -> Result<()> {
    let wallet = WalletView::deserialize(body).unwrap_or_default();
    if wallet.environment_id != Some(scope.single_env()?) {
        return Err(Error::usage(
            "Wallet does not belong to the selected environment",
        ));
    }
    Ok(())
}

/// Wallets have organization scope, so a supplied environment is checked against the
/// wallet before any mutation rather than trusted.
async fn verify_wallet_environment(api: &Api, scope: &Scope, wallet: Option<Uuid>) -> Result<()> {
    let wallet = wallet.ok_or_else(|| Error::usage("Missing wallet_id"))?;
    let path = format!("/organizations/{}/wallets/{wallet}", scope.require_org()?);
    let response = api.read(&path).await?;
    require_wallet_in_environment(&response.body, scope)
}

const PAYMENT_FEE_LIMIT_NOTE: &str = "Network/provider fee limits exclude additional processing fees. The wallet determines the network.";

/// What the recovery journal remembers about a submission: never the body itself.
#[derive(Debug, Serialize, Deserialize)]
struct JournalRecord {
    resource_id: Uuid,
    operation: OperationId,
    organization_id: Option<Uuid>,
    environment_ids: Vec<Uuid>,
    request_sha256: String,
    created_at: u64,
}

impl JournalRecord {
    /// The same ID may be resubmitted only with the same request in the same scope.
    fn describes_same_request(&self, other: &Self) -> bool {
        self.request_sha256 == other.request_sha256
            && self.organization_id == other.organization_id
            && self.environment_ids == other.environment_ids
            && self.operation == other.operation
    }
}

/// Record an ID-bearing mutation before submission so an ambiguous outcome can be reconciled.
fn journal(
    settings: &Settings,
    operation: &Operation,
    scope: &Scope,
    body: &Value,
    id: Uuid,
    terminal: Terminal,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    private_dir(&settings.dir)?;
    let dir = settings.dir.join(JOURNAL_DIR);
    private_dir(&dir)?;
    let record = JournalRecord {
        resource_id: id,
        operation: operation.id,
        organization_id: scope.org,
        environment_ids: scope.envs.clone(),
        request_sha256: hex::encode(Sha256::digest(serde_json::to_vec(body)?)),
        created_at: auth::now(),
    };
    let path = dir.join(format!("{id}.json"));
    if path.exists() {
        let previous: JournalRecord = serde_json::from_str(&read_private(&path)?)?;
        if !previous.describes_same_request(&record) {
            return Err(Error::usage(
                "This resource ID was previously used for a different request; reconcile it before proceeding",
            ));
        }
        return Ok(());
    }
    let mut file = new_private(&path)?;
    file.write_all(&serde_json::to_vec(&record)?)?;
    file.sync_all()?;
    terminal.important(format!(
        "Resource ID: {id}. Recovery record: {}",
        path.display()
    ));
    Ok(())
}

/// What this command may have changed, for reporting an interruption. A write is recorded
/// immediately before the request leaves the process, never during validation,
/// or wallet preflight: once sending starts, a cancelled future cannot tell whether the
/// service received the request.
#[derive(Default)]
pub struct SubmissionState {
    write: OnceLock<SubmittedWrite>,
    /// A change to a saved login whose server side may be done but whose local side is not.
    /// Cleared only once the local side is done, never on drop: Ctrl-C drops the command
    /// before the interruption is reported.
    session: Mutex<Option<SessionChange>>,
}

/// A login change that the server and the saved credentials can disagree about.
pub(crate) enum SessionChange {
    /// The single-use refresh token may be spent, but its replacement is not yet saved.
    Refresh { account: String },
    /// The server may have issued a session that is not yet saved.
    Login,
    /// The server may have revoked the session, but the saved login is not yet removed.
    Logout { account: String },
}

struct SubmittedWrite {
    operation: OperationId,
    resource_id: Option<Uuid>,
    organization_id: Option<Uuid>,
    environment_ids: Vec<Uuid>,
}

impl SubmissionState {
    /// A command sends at most one mutation, so the first record is the only one.
    fn record(&self, operation: OperationId, resource_id: Option<Uuid>, scope: &Scope) {
        let _ = self.write.set(SubmittedWrite {
            operation,
            resource_id,
            organization_id: scope.org,
            environment_ids: scope.envs.clone(),
        });
    }

    pub(crate) fn session_changing(&self, change: SessionChange) {
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = Some(change);
    }

    pub(crate) fn session_settled(&self) {
        *self.session.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// The error for Ctrl-C. It claims a write may continue only when one may have been sent,
    /// and never claims that the write was cancelled. A sent write outranks a session change,
    /// which always comes first.
    pub fn interrupted(&self) -> Error {
        let Some(write) = self.write.get() else {
            let session = self.session.lock().unwrap_or_else(PoisonError::into_inner);
            return match &*session {
                Some(SessionChange::Refresh { account }) => Error::interrupted(format!(
                    "Interrupted while refreshing the saved login for {account}"
                ))
                .with_hint(
                    "If the next command reports an expired session, run voltage login again.",
                ),
                Some(SessionChange::Login) => Error::interrupted(
                    "Interrupted after sign-in may have created a session that was not saved",
                )
                .with_hint(
                    "Run voltage login again; account global signout ends an unsaved session.",
                ),
                Some(SessionChange::Logout { account }) => Error::interrupted(format!(
                    "Interrupted while logging out {account}; its session may already be revoked"
                ))
                .with_hint("Rerun the logout with --local to remove the saved login."),
                None => Error::interrupted("Interrupted before any resource change was submitted"),
            };
        };
        let message = match write.resource_id {
            Some(id) if write.operation.submits_payment() => format!(
                "Interrupted after payment {id} may have been submitted; query its original ID before resubmitting. The payment was not cancelled."
            ),
            _ => "Interrupted after the request may have been submitted; check its result before retrying. The request was not cancelled.".to_owned(),
        };
        Error::interrupted(message).with_detail(ErrorDetail::uncertain_submission(
            write.resource_id,
            write.organization_id,
            write.environment_ids.clone(),
        ))
    }
}

/// A read can establish acceptance without sending the mutation again. A missing
/// projection remains uncertain and keeps the original recovery record.
async fn reconcile_payment(api: &Api, request: &Request, scope: &Scope) -> Option<Response> {
    let id = request.resource_id?;
    let org = scope.org?;
    let env = scope.single_env().ok()?;
    let path = format!("/organizations/{org}/environments/{env}/payments/{id}");
    let found = tokio::time::timeout(RECONCILE_TIMEOUT, api.read(&path))
        .await
        .ok()?
        .ok()?;
    if PaymentView::from_body(&found.body).id != Some(id) {
        return None;
    }
    api.terminal.important(format!(
        "Payment {id} is visible after the interrupted submission; reconciled by reading its original ID."
    ));
    Some(found)
}

/// Poll the payment until the wait target, pacing by the server's retry hints.
async fn wait_for_payment(
    steps: &Steps<'_>,
    request: &Request,
    mut response: Response,
    until: WaitTarget,
    out: &mut Output,
) -> Result<()> {
    let Steps {
        api,
        invocation,
        scope,
        deadline,
    } = *steps;
    let id = request
        .resource_id
        .ok_or_else(|| Error::usage("Waiting requires a payment ID"))?;
    let path = format!(
        "/organizations/{}/environments/{}/payments/{id}",
        scope.require_org()?,
        scope.single_env()?
    );
    let target = until.as_str();
    if invocation.operation.method != Method::Get {
        api.terminal
            .important(format!("Payment {id} accepted; waiting for {target}."));
    }
    let mut backoff = Backoff::new(
        match until {
            WaitTarget::Ready => READY_POLL_START,
            WaitTarget::Completed => COMPLETION_POLL_START,
        },
        POLL_CEILING,
    );
    let mut pause = backoff.pause();
    let mut invoice_presented = false;
    let mut last_status: Option<StatusText> = None;
    let mut last_notice = Instant::now() - STATUS_NOTICE_INTERVAL;
    loop {
        let view = PaymentView::from_body(&response.body);
        if let Some(status) = &view.status
            && (Some(status) != last_status.as_ref()
                || last_notice.elapsed() >= STATUS_NOTICE_INTERVAL)
        {
            api.terminal.notice(format!(
                "Payment {id} status: {}; waiting for {target}.",
                status.as_str()
            ));
            last_status = Some(status.clone());
            last_notice = Instant::now();
        }
        if invocation.presents_invoice()
            && !invoice_presented
            && let Some(invoice) = view.invoice()
        {
            present_invoice(invoice, invocation, api.terminal)?;
            invoice_presented = true;
            if until == WaitTarget::Completed {
                api.terminal
                    .notice("Invoice is ready; continuing to poll for settlement.");
            }
        }
        match view.progress(until) {
            WaitProgress::Unsuccessful => {
                return Err(Error::api("Payment reached an unsuccessful terminal state")
                    .with_detail(ErrorDetail::Payment(response.body))
                    .redacted(&api.secrets()));
            }
            WaitProgress::Reached(outcome) => {
                if invocation.presents_invoice() && !invoice_presented {
                    present_invoice(
                        view.invoice().ok_or_else(|| {
                            Error::usage("The ready payment does not contain a BOLT11 invoice")
                        })?,
                        invocation,
                        api.terminal,
                    )?;
                }
                return out.write(
                    Envelope::new(Some(response.status), response.body, Some(id), outcome),
                    &api.secrets(),
                );
            }
            WaitProgress::Pending => {}
        }
        if Instant::now() + pause >= deadline {
            return Err(Error::timeout(format!(
                "Payment {id} is still pending; waiting timed out"
            ))
            .with_detail(ErrorDetail::pending(id)));
        }
        tokio::time::sleep(pause).await;
        match tokio::time::timeout_at(deadline, api.read(&path)).await {
            Ok(Ok(next)) => {
                pause = next.retry_after.unwrap_or_else(|| backoff.pause());
                response = next;
            }
            Ok(Err(error)) if error.http_status() == Some(404) => pause = backoff.pause(),
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(Error::timeout(format!("Payment {id} wait timed out"))
                    .with_detail(ErrorDetail::pending(id)));
            }
        }
    }
}

/// Copy and render a ready BOLT11 invoice as requested; both go to stderr.
fn present_invoice(invoice: &str, invocation: &ApiInvocation, terminal: Terminal) -> Result<()> {
    if invocation.copy {
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(invoice)) {
            Ok(()) => terminal.notice("Invoice copied to the clipboard."),
            Err(_) => terminal.important(
                "Warning: the invoice is ready, but it could not be copied to the clipboard.",
            ),
        }
    }
    if invocation.qr {
        // BOLT11 is case-insensitive. Uppercase enables QR alphanumeric mode, while
        // low error correction and a two-module margin keep terminal output compact.
        let code = qrcode::QrCode::with_error_correction_level(
            invoice.to_ascii_uppercase(),
            qrcode::EcLevel::L,
        )
        .map_err(|_| Error::transport("Could not encode the invoice as a QR code"))?;
        let image = code
            .render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(false)
            .build();
        let image = image
            .lines()
            .map(|line| format!("  {line}  "))
            .collect::<Vec<_>>()
            .join("\n");
        terminal.important(format!("\nScan to pay:\n\n{image}\n"));
    }
    Ok(())
}

/// A checkout session read answers 202 until its projection exists.
async fn await_session_projection(
    api: &Api,
    request: &Request,
    mut response: Response,
    deadline: Instant,
) -> Result<Response> {
    let mut backoff = Backoff::new(SESSION_POLL_START, POLL_CEILING);
    while response.status == 202 {
        let pause = response.retry_after.unwrap_or_else(|| backoff.pause());
        if Instant::now() + pause >= deadline {
            return Err(Error::timeout("Checkout session projection is not ready"));
        }
        api.terminal
            .during(
                "Waiting for checkout session projection...",
                tokio::time::sleep(pause),
            )
            .await;
        response = tokio::time::timeout_at(
            deadline,
            api.send(Method::Get, &request.path, &request.query, None),
        )
        .await
        .map_err(|_| Error::timeout("Checkout session wait timed out"))??;
    }
    Ok(response)
}

#[derive(Serialize)]
struct Pages {
    pages: Vec<Envelope>,
}

/// Write the response, then follow pages when `follow`: collected for JSON, streamed for
/// NDJSON. A 202 is reported as accepted; any other status as `settled`.
async fn write_pages(
    steps: &Steps<'_>,
    request: &mut Request,
    mut response: Response,
    settled: Outcome,
    follow: bool,
    out: &mut Output,
) -> Result<()> {
    let Steps { api, deadline, .. } = *steps;
    let mut pages = Vec::new();
    let mut cursors = BTreeSet::new();
    loop {
        let next = if follow {
            next_page(&response.body, request)?
        } else {
            None
        };
        let outcome = if response.status == 202 {
            Outcome::Accepted
        } else {
            settled
        };
        let envelope = Envelope::new(
            Some(response.status),
            response.body,
            request.resource_id,
            outcome,
        );
        if follow && out.collects_pages() {
            pages.push(envelope);
        } else {
            out.write(envelope, &api.secrets())?;
        }
        let Some((name, value)) = next else {
            break;
        };
        if !cursors.insert((name.clone(), value.clone())) {
            return Err(Error::transport("Pagination did not advance"));
        }
        request.advance(name, value);
        response = tokio::time::timeout_at(
            deadline,
            api.send(Method::Get, &request.path, &request.query, None),
        )
        .await
        .map_err(|_| Error::timeout("Pagination deadline exceeded; use a longer --timeout"))??;
    }
    if follow && out.collects_pages() {
        out.write(
            Envelope::new(
                Some(200),
                serde_json::to_value(Pages { pages })?,
                None,
                Outcome::Retrieved,
            ),
            &api.secrets(),
        )?;
    }
    Ok(())
}

fn array_len<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<usize>, D::Error> {
    Option::<Vec<serde::de::IgnoredAny>>::deserialize(deserializer)
        .map(|items| items.map(|items| items.len()))
}

/// Read view over a list page for the fields that decide the next request.
#[derive(Debug, Default, Deserialize)]
struct PageView {
    #[serde(default)]
    has_more: Option<bool>,
    #[serde(default)]
    next_cursor: Option<String>,
    #[serde(default, deserialize_with = "array_len")]
    items: Option<usize>,
    #[serde(default, deserialize_with = "array_len")]
    entries: Option<usize>,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

/// The query change that fetches the next page, or `None` at the end.
fn next_page(body: &Value, request: &Request) -> Result<Option<(String, String)>> {
    let page = PageView::deserialize(body).unwrap_or_default();
    if request.pagination_mode() == PaginationMode::Cursor
        && (page.has_more.is_some() || page.next_cursor.is_some())
    {
        if page.has_more == Some(false) {
            return Ok(None);
        }
        if let Some(cursor) = page.next_cursor.filter(|cursor| !cursor.is_empty()) {
            return Ok(Some(("cursor".into(), cursor)));
        }
        if page.has_more == Some(true) {
            return Err(Error::transport(
                "API indicates more pages but returned no cursor",
            ));
        }
        return Ok(None);
    }
    if page.has_more == Some(false) {
        return Ok(None);
    }
    let Some(count) = page.items.or(page.entries).filter(|count| *count > 0) else {
        return Ok(None);
    };
    let offset = page.offset.or_else(|| request.offset()).unwrap_or(0);
    let next = offset
        .checked_add(count as u64)
        .ok_or_else(|| Error::transport("Pagination overflow"))?;
    if page.total.is_some_and(|total| next >= total)
        || page.limit.is_some_and(|limit| (count as u64) < limit)
    {
        return Ok(None);
    }
    Ok(Some(("offset".into(), next.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cli::{Command, Invocation},
        config::Config,
        registry::operation,
    };
    use reqwest::header::{HeaderMap, HeaderValue};
    use serde_json::json;

    const ORG: &str = "11111111-1111-4111-8111-111111111111";
    const ENV: &str = "22222222-2222-4222-8222-222222222222";
    const WALLET: &str = "33333333-3333-4333-8333-333333333333";

    fn scope() -> Scope {
        Scope {
            org: Some(ORG.parse().unwrap()),
            envs: vec![ENV.parse().unwrap()],
            wallet: Some(WALLET.parse().unwrap()),
            webhook: None,
            account: None,
        }
    }

    fn invocation(args: &[&str]) -> ApiInvocation {
        let mut full = vec!["voltage"];
        full.extend_from_slice(args);
        match Invocation::try_parse_from(full).unwrap().command {
            Command::Api(api) => *api,
            Command::Local(_) => panic!("expected an API command"),
        }
    }

    fn list_request(args: &[&str]) -> Request {
        Request::build(&invocation(args), &scope(), None).unwrap()
    }

    fn api(authorization: Authorization) -> Api {
        Api {
            client: auth::client(Duration::from_secs(5)).unwrap(),
            base: "https://api.example.test/v1".into(),
            authorization,
            origin: None,
            terminal: Terminal::new(false, false),
        }
    }

    #[test]
    fn authorization_presents_each_credential_where_the_api_expects_it() {
        let build = |authorization: Authorization| {
            api(authorization)
                .request(reqwest::Method::GET, "/wallets", &[], None)
                .build()
                .unwrap()
        };
        let key = build(Authorization::Account(ApiCredential::ApiKey(Secret::new(
            "key".into(),
        ))));
        assert_eq!(key.headers()["x-api-key"], "key");
        assert!(key.headers().get("authorization").is_none());
        let org = build(Authorization::Account(ApiCredential::OrganizationToken(
            Secret::new("org".into()),
        )));
        assert_eq!(org.headers()["authorization"], "Bearer org");
        let session = build(Authorization::CheckoutSession(Secret::new(
            "session".into(),
        )));
        assert_eq!(session.headers()["authorization"], "Bearer session");
        assert!(session.headers().get("x-api-key").is_none());
        let stream = build(Authorization::CheckoutStream(Secret::new("stream".into())));
        assert_eq!(stream.url().query(), Some("stream_token=stream"));
        assert!(stream.headers().get("authorization").is_none());
        let public = build(Authorization::None);
        assert!(public.headers().get("authorization").is_none() && public.url().query().is_none());
        assert_eq!(
            api(Authorization::CheckoutStream(Secret::new("stream".into()))).secrets(),
            ["stream"]
        );
    }

    #[test]
    fn disabled_features_are_explained_and_other_rejections_stay_generic() {
        let disabled = json!({"error": {
            "type": "feature_flag_disabled", "code": "disabled",
            "detail": "OnChain payments are not enabled for this organization",
            "context": {"feature": "on_chain"}
        }});
        assert_eq!(
            rejection_message(400, &disabled),
            "Voltage API returned HTTP 400: OnChain payments are not enabled for this organization. Ask Voltage to enable this feature for the organization."
        );
        let other = json!({"error": {"type": "invalid_amount", "detail": "too small"}});
        assert_eq!(
            rejection_message(400, &other),
            "Voltage API returned HTTP 400"
        );
        assert_eq!(
            rejection_message(502, &Value::Null),
            "Voltage API returned HTTP 502"
        );
    }

    #[test]
    fn retry_hints_prefer_milliseconds_then_seconds_then_dates() {
        let mut headers = HeaderMap::new();
        assert_eq!(retry_after(&headers), None);
        headers.insert("retry-after", HeaderValue::from_static("2"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(2)));
        headers.insert("x-retry-after-ms", HeaderValue::from_static("250"));
        assert_eq!(retry_after(&headers), Some(Duration::from_millis(250)));
        headers.remove("x-retry-after-ms");
        headers.insert(
            "retry-after",
            HeaderValue::from_static("Thu, 01 Jan 1970 00:00:00 GMT"),
        );
        assert_eq!(retry_after(&headers), Some(Duration::ZERO));
    }

    #[test]
    fn retry_hints_are_clamped_so_they_cannot_overflow_an_instant() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-retry-after-ms",
            HeaderValue::from_static("18446744073709551615"),
        );
        assert_eq!(retry_after(&headers), Some(MAX_RETRY_HINT));
        headers.remove("x-retry-after-ms");
        headers.insert(
            "retry-after",
            HeaderValue::from_static("18446744073709551615"),
        );
        assert_eq!(retry_after(&headers), Some(MAX_RETRY_HINT));
        headers.insert("retry-after", HeaderValue::from_static("10"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(10)));
    }

    #[test]
    fn cursor_pagination_stops() {
        let request = list_request(&["payments", "list"]);
        assert!(
            next_page(&json!({"has_more":false}), &request)
                .unwrap()
                .is_none()
        );
        assert!(next_page(&json!({"has_more":true}), &request).is_err());
        assert_eq!(
            next_page(&json!({"has_more":true,"next_cursor":"abc"}), &request).unwrap(),
            Some(("cursor".into(), "abc".into()))
        );
        assert!(
            next_page(&json!({"items":[1,2]}), &request)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn offset_pagination_advances_by_item_count_until_the_page_is_short() {
        let request = list_request(&[
            "payments",
            "list",
            "--query",
            "offset=20",
            "--query",
            "limit=2",
        ]);
        assert_eq!(request.pagination_mode(), PaginationMode::Offset);
        assert_eq!(
            next_page(&json!({"items":[1,2],"has_more":true}), &request).unwrap(),
            Some(("offset".into(), "22".into()))
        );
        assert_eq!(
            next_page(&json!({"entries":[1,2],"offset":40}), &request).unwrap(),
            Some(("offset".into(), "42".into()))
        );
        assert!(
            next_page(&json!({"items":[1],"limit":2}), &request)
                .unwrap()
                .is_none()
        );
        assert!(
            next_page(&json!({"items":[1,2],"total":22}), &request)
                .unwrap()
                .is_none()
        );
        assert!(next_page(&json!({"items":[]}), &request).unwrap().is_none());
        assert!(
            next_page(&json!({"has_more":false,"items":[1,2]}), &request)
                .unwrap()
                .is_none()
        );
        assert!(next_page(&json!({"items":[1],"offset":u64::MAX}), &request).is_err());
    }

    #[test]
    fn events_assemble_from_lines_and_dispatch_on_blank_lines() {
        let mut assembler = EventAssembler::default();
        assert!(assembler.line("event: updated").unwrap().is_none());
        assert!(assembler.line("id: 7").unwrap().is_none());
        assert!(assembler.line("data: {\"status\":").unwrap().is_none());
        assert!(assembler.line("data:  \"completed\"}").unwrap().is_none());
        assert!(assembler.line(": comment").unwrap().is_none());
        let envelope = assembler.line("").unwrap().unwrap();
        assert_eq!(
            serde_json::to_value(envelope).unwrap(),
            json!({"http_status":200,"data":{"status":"completed"},"resource_id":null,"outcome":"event","event":"updated","id":"7"})
        );
        assert!(
            assembler.line("").unwrap().is_none(),
            "blank lines without data emit nothing"
        );
        assert!(assembler.line("data: plain text").unwrap().is_none());
        let envelope = assembler.line("").unwrap().unwrap();
        assert_eq!(envelope.data, json!("plain text"));
        assert_eq!(
            envelope.event.as_ref().map(|event| event.event.as_str()),
            Some("message")
        );
        let mut oversized = EventAssembler::default();
        let error = oversized
            .line(&format!("data: {}", "x".repeat(MAX_EVENT_BYTES + 1)))
            .unwrap_err();
        assert_eq!(error.message, "Event exceeds 1 MiB");
    }

    #[test]
    fn interruption_claims_a_possible_submission_only_after_a_write_starts() {
        let submission = SubmissionState::default();
        let before = submission.interrupted();
        assert_eq!(before.kind, ErrorKind::Interrupted);
        assert_eq!(
            before.message,
            "Interrupted before any resource change was submitted"
        );
        assert!(before.detail.is_none());

        let id = Uuid::nil();
        submission.record(OperationId::CreatePayment, Some(id), &scope());
        // Only the first write is the command's submission.
        submission.record(OperationId::DeleteWallet, None, &scope());
        let after = submission.interrupted();
        assert!(
            after
                .message
                .contains(&format!("payment {id} may have been submitted"))
        );
        assert!(after.message.contains("was not cancelled"));
        let detail = serde_json::to_value(after.detail.unwrap()).unwrap();
        assert_eq!(detail["resource_id"], json!(id));
        assert_eq!(detail["organization_id"], json!(ORG));
        assert_eq!(detail["outcome"], "unknown");
    }

    #[test]
    fn interruption_during_a_session_change_says_how_to_recover_until_it_settles() {
        let submission = SubmissionState::default();
        for (change, message, hint) in [
            (
                SessionChange::Refresh {
                    account: "person".into(),
                },
                "Interrupted while refreshing the saved login for person",
                "voltage login",
            ),
            (
                SessionChange::Login,
                "Interrupted after sign-in may have created a session that was not saved",
                "voltage login",
            ),
            (
                SessionChange::Logout {
                    account: "person".into(),
                },
                "Interrupted while logging out person; its session may already be revoked",
                "--local",
            ),
        ] {
            submission.session_changing(change);
            let during = submission.interrupted();
            assert_eq!(during.message, message);
            assert!(during.hint.unwrap().contains(hint));
            assert!(during.detail.is_none());

            submission.session_settled();
            let after = submission.interrupted();
            assert_eq!(
                after.message,
                "Interrupted before any resource change was submitted"
            );
        }

        // A sent write outranks a session change that has not settled.
        submission.session_changing(SessionChange::Refresh {
            account: "person".into(),
        });
        submission.record(OperationId::DeleteWallet, None, &scope());
        assert!(
            submission
                .interrupted()
                .message
                .contains("may have been submitted")
        );
    }

    #[test]
    fn interruption_after_another_write_is_uncertain_without_naming_a_payment() {
        let submission = SubmissionState::default();
        submission.record(OperationId::DeleteWallet, None, &scope());
        let error = submission.interrupted();
        assert!(!error.message.contains("payment"));
        assert!(error.message.contains("may have been submitted"));
        let detail = serde_json::to_value(error.detail.unwrap()).unwrap();
        assert!(detail["resource_id"].is_null());
        assert_eq!(detail["outcome"], "unknown");
    }

    #[test]
    fn journal_rejects_the_same_id_for_a_different_request() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let settings = Settings {
            dir: dir.path().to_path_buf(),
            config: Config::default(),
        };
        let create_payment = operation(OperationId::CreatePayment);
        let id = Uuid::nil();
        let body = json!({"id": id, "wallet_id": WALLET});
        let terminal = Terminal::new(false, false);
        journal(&settings, create_payment, &scope(), &body, id, terminal).unwrap();
        journal(&settings, create_payment, &scope(), &body, id, terminal).unwrap();
        let changed = json!({"id": id, "wallet_id": ORG});
        let error =
            journal(&settings, create_payment, &scope(), &changed, id, terminal).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Usage);
        let other_scope = Scope {
            envs: Vec::new(),
            ..scope()
        };
        assert!(journal(&settings, create_payment, &other_scope, &body, id, terminal).is_err());
        let record: JournalRecord = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(JOURNAL_DIR).join(format!("{id}.json")))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(record.operation, OperationId::CreatePayment);
        assert!(
            !std::fs::read_to_string(dir.path().join(JOURNAL_DIR).join(format!("{id}.json")))
                .unwrap()
                .contains(WALLET)
        );
    }

    #[test]
    fn reads_plan_how_they_finish_and_only_an_explicit_wait_tolerates_a_missing_payment() {
        let read = |args: &[&str]| Read::plan(&invocation(args), &scope());
        assert_eq!(
            read(&["payments", "get", WALLET, "--wait", "completed"]).then,
            ReadThen::PaymentWait {
                until: WaitTarget::Completed,
                missing_is_pending: true,
            }
        );
        assert_eq!(
            read(&["payments", "get", WALLET, "--qr"]).then,
            ReadThen::PaymentWait {
                until: WaitTarget::Ready,
                missing_is_pending: false,
            }
        );
        assert_eq!(
            read(&["checkout", "sessions", "get", WALLET]).then,
            ReadThen::SessionProjection
        );
        assert_eq!(
            read(&["payments", "list", "--all"]).then,
            ReadThen::Pages { follow: true }
        );
        assert!(read(&["wallets", "get", WALLET]).check_wallet_environment);
        assert!(!read(&["wallets", "list"]).check_wallet_environment);
    }

    #[test]
    fn the_flag_decides_then_voltage_execute_then_reads_send_and_changes_are_described() {
        let resolve = |args: &[&str], variable| Execution::resolve(&invocation(args), variable);
        let change = ["wallets", "delete", WALLET];
        let dry_run = ["wallets", "delete", WALLET, "--dry-run"];
        let execute = ["wallets", "delete", WALLET, "--execute"];
        for variable in [None, Some(false), Some(true)] {
            assert_eq!(resolve(&dry_run, variable), Execution::DryRun);
            assert_eq!(resolve(&execute, variable), Execution::Send);
            assert_eq!(resolve(&["wallets", "list"], variable), Execution::Send);
        }
        assert_eq!(resolve(&change, None), Execution::DescribeChange);
        assert_eq!(resolve(&change, Some(false)), Execution::DescribeChange);
        assert_eq!(resolve(&change, Some(true)), Execution::SendFromEnvironment);
        assert!(Execution::DescribeChange.describes() && Execution::DryRun.describes());
        assert!(!Execution::SendFromEnvironment.describes());
    }

    #[test]
    fn run_encoding_is_stable() {
        let mutation = Run::Mutation(Mutation {
            verify_wallet_environment: true,
            journal_id: Some(WALLET.parse().unwrap()),
            reconcile: true,
            wait: Some(WaitTarget::Ready),
        });
        assert_eq!(
            serde_json::to_value(mutation).unwrap(),
            json!({
                "kind": "mutation",
                "verify_wallet_environment": true,
                "journal_id": WALLET,
                "reconcile": true,
                "wait": "ready"
            })
        );
        let read = Run::Read(Read {
            check_wallet_environment: false,
            then: ReadThen::PaymentWait {
                until: WaitTarget::Completed,
                missing_is_pending: true,
            },
        });
        assert_eq!(
            serde_json::to_value(read).unwrap(),
            json!({
                "kind": "read",
                "check_wallet_environment": false,
                "then": {"kind": "payment_wait", "until": "completed", "missing_is_pending": true}
            })
        );
        assert_eq!(
            serde_json::to_value(Run::EventStream).unwrap(),
            json!({"kind": "event_stream"})
        );
    }

    #[test]
    fn invoice_flags_apply_only_to_bolt11_receives_and_payment_reads() {
        let receive = invocation(&[
            "payments",
            "receive",
            "--currency",
            "btc",
            "--kind",
            "bolt11",
            "--qr",
        ]);
        let request = Request::build(&receive, &scope(), None).unwrap();
        assert!(check_invoice_flags(&receive, &request).is_ok());
        let onchain = invocation(&[
            "payments",
            "receive",
            "--currency",
            "btc",
            "--kind",
            "onchain",
            "--copy",
        ]);
        let request = Request::build(&onchain, &scope(), None).unwrap();
        assert_eq!(
            check_invoice_flags(&onchain, &request).unwrap_err().message,
            "--qr and --copy require a BOLT11 receive (--kind bolt11)"
        );
        let create = invocation(&["payments", "create", "--data", "-", "--qr"]);
        let error = check_invoice_flags(&create, &list_request(&["payments", "list"])).unwrap_err();
        assert_eq!(
            error.message,
            "--qr and --copy are only available for payments receive and payments get"
        );
        let read = invocation(&["payments", "get", WALLET, "--qr"]);
        assert!(check_invoice_flags(&read, &list_request(&["payments", "list"])).is_ok());
    }

    #[tokio::test]
    async fn response_bodies_are_bounded() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw(vec![b'x'; 64], "application/octet-stream"),
            )
            .mount(&server)
            .await;
        let response = reqwest::get(server.uri()).await.unwrap();
        assert!(matches!(
            bounded_body(response, 16).await,
            Err(BodyFailure::TooLarge)
        ));
        let response = reqwest::get(server.uri()).await.unwrap();
        assert_eq!(
            bounded_body(response, 64).await.ok().map(|body| body.len()),
            Some(64)
        );
    }
}
