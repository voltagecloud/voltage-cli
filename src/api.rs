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
    cli::{ApiInvocation, GlobalFlags, Origin},
    config::{API_URL, Scope, Settings, new_private, private_dir, read_private, read_secret},
    error::{ErrorDetail, ErrorKind},
    input::{PaginationMode, Request},
    output::{Envelope, Outcome, Output, redact},
    payment::{PaymentDirection, PaymentView, ReceiveKind, StatusText, WaitProgress, WaitTarget},
    registry::{AuthScheme, Method, Operation, OperationId},
    secret::Secret,
};
use futures_util::StreamExt;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    io::{IsTerminal, Write},
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
    ) -> Result<Self> {
        let scheme = invocation.operation.auth;
        let checkout_token = |variable: &str| -> Result<Secret> {
            if let Some(source) = &invocation.token_file {
                return read_secret(source);
            }
            std::env::var(variable)
                .ok()
                .map(Secret::new)
                .filter(|secret| !secret.expose().trim().is_empty())
                .ok_or_else(|| Error::auth(format!("Supply --token-file or {variable}")))
        };
        Ok(match scheme {
            AuthScheme::Account => {
                Self::Account(auth::resolve_organization(settings, scope, global).await?)
            }
            AuthScheme::CheckoutSession => {
                Self::CheckoutSession(checkout_token("VOLTAGE_CHECKOUT_TOKEN")?)
            }
            AuthScheme::CheckoutStream => {
                Self::CheckoutStream(checkout_token("VOLTAGE_STREAM_TOKEN")?)
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
            .request(reqwest::Method::GET, &request.path, &request.query, None)
            .send()
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
        while let Some(chunk) = stream.next().await {
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

/// `x-retry-after-ms` wins; `retry-after` may be seconds or an HTTP date.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let text = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    if let Some(millis) = text("x-retry-after-ms").and_then(|value| value.parse::<u64>().ok()) {
        return Some(Duration::from_millis(millis));
    }
    let value = text("retry-after")?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    httpdate::parse_http_date(value).ok().map(|at| {
        at.duration_since(std::time::SystemTime::now())
            .unwrap_or_default()
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
    global: &GlobalFlags,
    settings: &Settings,
    scope: &Scope,
    out: &mut Output,
) -> Result<()> {
    let operation = invocation.operation;
    let mut request = Request::build(invocation, scope)?;
    check_invoice_flags(invocation, &request)?;
    if operation.id.returns_one_time_secret() && !out.secure_destination() {
        return Err(Error::usage(
            "This operation returns a one-time secret; supply --output-file PATH or --show-secrets before executing",
        ));
    }
    let authorization = Authorization::resolve(invocation, global, settings, scope).await?;
    let api = Api::new(global, invocation, authorization)?;
    let deadline = Instant::now() + global.timeout;
    if request.pagination_mode() == PaginationMode::Offset && operation.has_parameter("cursor") {
        eprintln!(
            "Offset pagination is deprecated by the API; omit --offset and --pagination to page by cursor."
        );
    }
    guard_submission(&api, invocation, &request, settings, scope, global).await?;
    if operation.auth == AuthScheme::CheckoutStream {
        return api.stream(&request, out).await;
    }
    let Submission {
        mut response,
        reconciled,
    } = submit(&api, invocation, &request, scope).await?;
    check_response(invocation, &response, scope)?;
    if let Some(until) = wait_target(invocation) {
        return wait_for_payment(
            &api, invocation, &request, scope, response, until, deadline, out,
        )
        .await;
    }
    if operation.id == OperationId::GetSession {
        response = await_session_projection(&api, &request, response, deadline).await?;
    }
    write_pages(
        &api,
        invocation,
        &mut request,
        response,
        reconciled,
        deadline,
        out,
    )
    .await
}

/// Everything that must hold before a request leaves the process, in order: a wallet
/// mutation's environment is verified against the wallet, consequential actions are
/// confirmed, and an ID-bearing mutation is journaled for recovery.
async fn guard_submission(
    api: &Api,
    invocation: &ApiInvocation,
    request: &Request,
    settings: &Settings,
    scope: &Scope,
    global: &GlobalFlags,
) -> Result<()> {
    let operation = invocation.operation;
    if operation.targets_wallet() && operation.method != Method::Get && !scope.envs.is_empty() {
        verify_wallet_environment(api, scope, invocation.resource_id).await?;
    }
    if request.is_consequential(operation) {
        confirm(operation, scope, request, global.yes)?;
    }
    if operation.method != Method::Get
        && let (Some(body), Some(id)) = (&request.body, request.body_id)
    {
        journal(settings, operation, scope, body, id)?;
    }
    Ok(())
}

/// `--qr` and `--copy` imply `--wait ready` when no explicit wait was given.
fn wait_target(invocation: &ApiInvocation) -> Option<WaitTarget> {
    invocation
        .wait
        .or_else(|| invocation.presents_invoice().then_some(WaitTarget::Ready))
}

/// Checks on the first response: a wallet read must sit in the selected environment, and
/// an invoice can only be presented for a receive.
fn check_response(invocation: &ApiInvocation, response: &Response, scope: &Scope) -> Result<()> {
    if invocation.operation.id == OperationId::GetWallet && !scope.envs.is_empty() {
        require_wallet_in_environment(&response.body, scope)?;
    }
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

#[derive(Serialize)]
struct ConfirmationSummary<'a> {
    action: String,
    organization: Option<Uuid>,
    environments: &'a [Uuid],
    resource_id: Option<Uuid>,
    request: Option<&'a Value>,
}

/// Show the resolved scope and request on stderr, then require `--yes` or a terminal answer.
fn confirm(operation: &Operation, scope: &Scope, request: &Request, yes: bool) -> Result<()> {
    let mut summary = serde_json::to_value(ConfirmationSummary {
        action: operation.command.join(" "),
        organization: scope.org,
        environments: &scope.envs,
        resource_id: request.resource_id,
        request: request.body.as_ref(),
    })?;
    redact(&mut summary, &[]);
    eprintln!("{}", serde_json::to_string_pretty(&summary)?);
    if operation.id == OperationId::CreatePayment {
        eprintln!(
            "Network/provider fee limits exclude additional processing fees. The wallet determines the network."
        );
    }
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(Error::usage(
            "This operation requires --yes in noninteractive use",
        ));
    }
    eprint!("Proceed? [y/N] ");
    std::io::stderr().flush()?;
    let mut reply = String::new();
    std::io::stdin().read_line(&mut reply)?;
    if !matches!(reply.trim(), "y" | "Y" | "yes") {
        return Err(Error::usage("Operation cancelled before submission"));
    }
    Ok(())
}

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
    eprintln!("Resource ID: {id}. Recovery record: {}", path.display());
    Ok(())
}

struct Submission {
    response: Response,
    /// The mutation's transport failed, but reading the original ID proved acceptance.
    reconciled: bool,
}

/// Send the request once. A payment whose transport failed is looked up by its ID within a
/// short bound; an explicit wait tolerates a projection that is not visible yet.
async fn submit(
    api: &Api,
    invocation: &ApiInvocation,
    request: &Request,
    scope: &Scope,
) -> Result<Submission> {
    let operation = invocation.operation;
    let result = api
        .send(
            operation.method,
            &request.path,
            &request.query,
            request.body.as_ref(),
        )
        .await;
    let submission = match result {
        Ok(response) => Ok(Submission {
            response,
            reconciled: false,
        }),
        Err(error)
            if operation.id == OperationId::GetPayment
                && invocation.wait.is_some()
                && error.http_status() == Some(404) =>
        {
            // A submitted payment can be missing from the read projection initially.
            // Only an explicit wait treats that 404 as pending; ordinary reads fail.
            Ok(Submission {
                response: Response {
                    status: 404,
                    body: Value::Null,
                    retry_after: None,
                },
                reconciled: false,
            })
        }
        Err(error) if error.is_transport() && operation.id.submits_payment() => {
            match reconcile_payment(api, request, scope).await {
                Some(response) => Ok(Submission {
                    response,
                    reconciled: true,
                }),
                None => Err(error),
            }
        }
        Err(error) => Err(error),
    };
    submission.map_err(|mut error| {
        if operation.method != Method::Get && error.is_transport() {
            error.detail = Some(ErrorDetail::uncertain_submission(
                request.resource_id,
                scope.org,
                scope.envs.clone(),
            ));
        }
        error
    })
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
    eprintln!(
        "Payment {id} is visible after the interrupted submission; reconciled by reading its original ID."
    );
    Some(found)
}

/// Poll the payment until the wait target, pacing by the server's retry hints.
#[expect(
    clippy::too_many_arguments,
    reason = "one wait has exactly these inputs"
)]
async fn wait_for_payment(
    api: &Api,
    invocation: &ApiInvocation,
    request: &Request,
    scope: &Scope,
    mut response: Response,
    until: WaitTarget,
    deadline: Instant,
    out: &mut Output,
) -> Result<()> {
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
        eprintln!("Payment {id} accepted; waiting for {target}.");
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
            eprintln!(
                "Payment {id} status: {}; waiting for {target}.",
                status.as_str()
            );
            last_status = Some(status.clone());
            last_notice = Instant::now();
        }
        if invocation.presents_invoice()
            && !invoice_presented
            && let Some(invoice) = view.invoice()
        {
            present_invoice(invoice, invocation)?;
            invoice_presented = true;
            if until == WaitTarget::Completed {
                eprintln!("Invoice is ready; continuing to poll for settlement.");
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
fn present_invoice(invoice: &str, invocation: &ApiInvocation) -> Result<()> {
    if invocation.copy {
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(invoice)) {
            Ok(()) => eprintln!("Invoice copied to the clipboard."),
            Err(_) => eprintln!(
                "Warning: the invoice is ready, but it could not be copied to the clipboard."
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
        eprintln!("\nScan to pay:\n\n{image}\n");
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
        tokio::time::sleep(pause).await;
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

/// Write the response, then follow pages under `--all`: collected for JSON, streamed for NDJSON.
async fn write_pages(
    api: &Api,
    invocation: &ApiInvocation,
    request: &mut Request,
    mut response: Response,
    reconciled: bool,
    deadline: Instant,
    out: &mut Output,
) -> Result<()> {
    let operation = invocation.operation;
    let mut pages = Vec::new();
    let mut cursors = BTreeSet::new();
    loop {
        let next = if invocation.all {
            next_page(&response.body, request)?
        } else {
            None
        };
        let outcome = if response.status == 202 || reconciled {
            Outcome::Accepted
        } else if operation.method == Method::Get {
            Outcome::Retrieved
        } else {
            Outcome::Succeeded
        };
        let envelope = Envelope::new(
            Some(response.status),
            response.body,
            request.resource_id,
            outcome,
        );
        if invocation.all && out.collects_pages() {
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
    if invocation.all && out.collects_pages() {
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
        Request::build(&invocation(args), &scope()).unwrap()
    }

    fn api(authorization: Authorization) -> Api {
        Api {
            client: auth::client(Duration::from_secs(5)).unwrap(),
            base: "https://api.example.test/v1".into(),
            authorization,
            origin: None,
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
        journal(&settings, create_payment, &scope(), &body, id).unwrap();
        journal(&settings, create_payment, &scope(), &body, id).unwrap();
        let changed = json!({"id": id, "wallet_id": ORG});
        let error = journal(&settings, create_payment, &scope(), &changed, id).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Usage);
        let other_scope = Scope {
            envs: Vec::new(),
            ..scope()
        };
        assert!(journal(&settings, create_payment, &other_scope, &body, id).is_err());
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
        let request = Request::build(&receive, &scope()).unwrap();
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
        let request = Request::build(&onchain, &scope()).unwrap();
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
