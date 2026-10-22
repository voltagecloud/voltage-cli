//! Browser device login, token refresh, organization token exchange, and credential selection.
//!
//! Token responses deserialize straight into `Secret` fields and the response buffer is
//! zeroized, so plaintext tokens never sit in a general-purpose `Value`. Token mutations are
//! never retried; only device polling backs off.

use crate::{
    Error, Result,
    api::{SessionChange, SubmissionState},
    cli::{GlobalFlags, ImportKeyFlags, LoginFlags},
    config::{
        AUTH_URL, Account, AccountKind, Credential, CredentialProblem, CredentialStore,
        InputSource, LOCK_TIMEOUT, Login, Scope, ScopeSources, Settings, Source, name_in_use,
        read_secret, shell_word, unreadable_name,
    },
    error::{CredentialRejection, CredentialSources, ErrorDetail, ResponseLimit},
    secret::Secret,
    terminal::Terminal,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const CLIENT_ID: &str = "voltage-cli";
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const TOKEN_EXCHANGE_GRANT: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
// A credential lock holder makes at most one auth request, so a waiter must outlast it.
const _: () = assert!(LOCK_TIMEOUT.as_secs() >= 2 * AUTH_TIMEOUT.as_secs());
/// Access tokens this close to expiry are refreshed before use.
const REFRESH_MARGIN: u64 = 30;
/// Device approval waits are bounded regardless of the server's `expires_in`.
const MAX_LOGIN_WAIT: u64 = 600;
const MIN_POLL_INTERVAL: u64 = 5;
const MAX_POLL_INTERVAL: u64 = 600;
const SLOW_DOWN_STEP: u64 = 5;
/// `TransportFailure::ConnectTimeout` names this limit in its message.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Auth responses are small JSON documents; anything larger is not one.
const MAX_AUTH_RESPONSE_BYTES: usize = 1024 * 1024;

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Service URLs require HTTPS (HTTP only on loopback) without credentials, query, or fragment.
pub fn base_url(raw: &str) -> Result<String> {
    let url = url::Url::parse(raw).map_err(|_| Error::usage("Invalid service URL"))?;
    let loopback = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::usage(
            "Service URLs require HTTPS (HTTP allowed only on loopback), without credentials, query, or fragment",
        ));
    }
    Ok(raw.trim_end_matches('/').into())
}

/// An HTTP client that never follows redirects or retries, so no credential or mutation
/// is resent without the CLI deciding to.
pub fn client(timeout: Duration) -> Result<reqwest::Client> {
    // reqwest is built without a default TLS provider; ring is installed once, before the
    // first client, and a second install attempt is the harmless "already installed" case.
    static PROVIDER: std::sync::Once = std::sync::Once::new();
    PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT)
        .user_agent(concat!("voltage-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| Error::transport("Cannot initialize HTTP client"))
}

/// Credential presented to the Voltage API for one command.
pub enum ApiCredential {
    ApiKey(Secret),
    /// An organization token exchanged from the login; never reused for another organization.
    OrganizationToken(Secret),
}

/// Which listing `discover` fetches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Discovery {
    Organizations,
    Environments,
}

/// A response from the auth service with its body in zeroized memory.
struct AuthResponse {
    status: u16,
    body: Zeroizing<Vec<u8>>,
}

#[derive(Deserialize)]
struct OAuthErrorBody {
    error: Option<String>,
}

impl AuthResponse {
    fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The server's OAuth error code when it is a plain identifier, never its free text.
    fn error_code(&self) -> Option<String> {
        serde_json::from_slice::<OAuthErrorBody>(&self.body)
            .ok()
            .and_then(|body| body.error)
            .filter(|code| {
                code.len() < 80 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
    }

    /// `require_success` for a request made with a saved login: OAuth reports an expired,
    /// revoked, or already spent grant as `invalid_grant`, so the error names the fix.
    fn require_login(&self, account: &str, url: &str) -> Result<()> {
        if !self.is_success() && self.error_code().as_deref() == Some("invalid_grant") {
            return Err(expired_login(account, url));
        }
        self.require_success()
    }

    /// Fail with the server's error code when it is a plain identifier, never its free text.
    fn require_success(&self) -> Result<()> {
        if self.is_success() {
            return Ok(());
        }
        let error = self
            .error_code()
            .unwrap_or_else(|| "authentication_failed".into());
        Err(Error::auth(format!(
            "Authentication failed (HTTP {}, {error})",
            self.status
        )))
    }

    fn json<T: DeserializeOwned>(&self, invalid: &'static str) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(|_| Error::auth(invalid))
    }
}

/// Append one transport chunk to zeroized memory, scrubbing the chunk when it is uniquely
/// owned.
fn absorb_chunk(body: &mut Zeroizing<Vec<u8>>, chunk: bytes::Bytes) {
    match chunk.try_into_mut() {
        Ok(mut unique) => {
            body.extend_from_slice(&unique);
            unique.as_mut().zeroize();
        }
        Err(shared) => body.extend_from_slice(&shared),
    }
}

/// Read a bounded response body into zeroized memory.
async fn bounded_body(mut response: reqwest::Response, limit: usize) -> Result<Zeroizing<Vec<u8>>> {
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Error::transport("Unable to read authentication response"))?
    {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(Error::auth("Authentication response exceeds 1 MiB"));
        }
        absorb_chunk(&mut body, chunk);
    }
    Ok(body)
}

async fn auth_request(
    request: reqwest::RequestBuilder,
    terminal: Terminal,
) -> Result<AuthResponse> {
    let _progress = terminal.progress("Waiting for auth service...");
    let response = request.send().await.map_err(|error| {
        Error::no_response(
            &error,
            "Authentication request",
            "; no token request was automatically retried",
            "auth",
            ResponseLimit::Fixed(AUTH_TIMEOUT),
            false,
        )
    })?;
    let status = response.status().as_u16();
    // Ingress rate limits can return plain text rather than an OAuth response.
    // Device polling still needs to back off; token mutations are never retried.
    let body = if status == 429 {
        Zeroizing::new(br#"{"error":"slow_down"}"#.to_vec())
    } else {
        bounded_body(response, MAX_AUTH_RESPONSE_BYTES).await?
    };
    if !body.is_empty() && serde_json::from_slice::<serde::de::IgnoredAny>(&body).is_err() {
        return Err(Error::auth("Invalid authentication response"));
    }
    Ok(AuthResponse { status, body })
}

/// The auth service at one base URL. Every call goes through `auth_request`, so none is
/// redirected or retried, and each response body stays in zeroized memory.
struct AuthService {
    client: reqwest::Client,
    url: String,
    terminal: Terminal,
}

impl AuthService {
    /// `url` is checked as a service URL.
    fn new(url: &str, terminal: Terminal) -> Result<Self> {
        Ok(Self {
            client: client(AUTH_TIMEOUT)?,
            url: base_url(url)?,
            terminal,
        })
    }

    async fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<AuthResponse> {
        auth_request(
            self.client.post(format!("{}{path}", self.url)).form(form),
            self.terminal,
        )
        .await
    }

    async fn get(&self, path: &str, token: &Secret) -> Result<AuthResponse> {
        auth_request(
            self.client
                .get(format!("{}{path}", self.url))
                .bearer_auth(token.expose()),
            self.terminal,
        )
        .await
    }

    async fn device_authorization(&self) -> Result<DeviceAuthorization> {
        let response = self
            .post("/oauth/device_authorization", &[("client_id", CLIENT_ID)])
            .await?;
        response.require_success()?;
        response.json("Invalid device authorization response")
    }

    /// Poll the token endpoint at the server's interval until approval, denial, or expiry.
    async fn poll_device_grant(
        &self,
        device: &DeviceAuthorization,
        submission: &SubmissionState,
    ) -> Result<TokenResponse> {
        let expires = device.expires_in.min(MAX_LOGIN_WAIT);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(expires);
        let mut interval = device
            .interval
            .unwrap_or(MIN_POLL_INTERVAL)
            .clamp(MIN_POLL_INTERVAL, MAX_POLL_INTERVAL);
        let started = tokio::time::Instant::now();
        loop {
            if tokio::time::Instant::now() + Duration::from_secs(interval) >= deadline {
                return Err(Error::timeout("Login expired; run voltage login again"));
            }
            self.terminal
                .waiting(
                    "Waiting for device approval...",
                    started,
                    tokio::time::sleep(Duration::from_secs(interval)),
                )
                .await;
            // Any poll can be the one that issues the session, which stays unsaved until login
            // finishes; between polls, nothing has been issued.
            submission.session_changing(SessionChange::Login);
            let response = tokio::time::timeout_at(
                deadline,
                self.post(
                    "/oauth/token",
                    &[
                        ("client_id", CLIENT_ID),
                        ("grant_type", DEVICE_CODE_GRANT),
                        ("device_code", device.device_code.expose()),
                    ],
                ),
            )
            .await
            .map_err(|_| Error::timeout("Login expired"))??;
            if response.is_success() {
                return response.json("Invalid token response");
            }
            submission.session_settled();
            let error = response
                .json::<DeviceGrantErrorBody>("Invalid authentication response")
                .ok()
                .and_then(|body| body.error);
            match error {
                Some(DeviceGrantError::AuthorizationPending) => {}
                Some(DeviceGrantError::SlowDown) => {
                    interval = interval.saturating_add(SLOW_DOWN_STEP);
                }
                Some(DeviceGrantError::ExpiredToken) => {
                    return Err(Error::timeout("Login expired"));
                }
                Some(DeviceGrantError::AccessDenied) => {
                    return Err(Error::auth("Login was denied"));
                }
                Some(DeviceGrantError::Other) | None => response.require_success()?,
            }
        }
    }

    async fn current_user(&self, access_token: &Secret) -> Result<CurrentUser> {
        let response = self.get("/users/current", access_token).await?;
        response.require_success()?;
        response.json("Invalid user response")
    }

    /// Rotate an expiring login; the refresh token is single use, so the caller saves the
    /// result before anything else sees it.
    async fn refresh(&self, login: Login, account: &str) -> Result<Login> {
        let response = self
            .post(
                "/oauth/token",
                &[
                    ("client_id", CLIENT_ID),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", login.refresh_token.expose()),
                ],
            )
            .await?;
        response.require_login(account, &self.url)?;
        let token: TokenResponse = response.json("Invalid token response")?;
        token.into_login(login.user_id, login.email)
    }

    /// Trade a login's account-wide access token (OAuth token exchange) for a short-lived
    /// token whose audience is one organization.
    async fn exchange(
        &self,
        access_token: &Secret,
        organization: Uuid,
        account: &str,
    ) -> Result<Secret> {
        let response = self
            .post(
                "/oauth/token",
                &[
                    ("grant_type", TOKEN_EXCHANGE_GRANT),
                    ("subject_token", access_token.expose()),
                    ("subject_token_type", ACCESS_TOKEN_TYPE),
                    ("audience", &organization.to_string()),
                ],
            )
            .await?;
        // A session revoked before its access token expires fails here, not at refresh.
        response.require_login(account, &self.url)?;
        let token: OrganizationTokenResponse =
            response.json("Invalid organization token response")?;
        if token.access_token.expose().is_empty()
            || token.issued_token_type != ACCESS_TOKEN_TYPE
            || !token.token_type.eq_ignore_ascii_case("bearer")
            || token.expires_in == 0
        {
            return Err(Error::auth("Invalid organization token response"));
        }
        Ok(token.access_token)
    }

    async fn organizations(&self, access_token: &Secret) -> Result<Value> {
        let response = self.get("/users/current", access_token).await?;
        response.require_success()?;
        Ok(response
            .json::<OrganizationDiscovery>("Invalid user response")?
            .organizations)
    }

    async fn environments(&self, organization_token: &Secret, organization: Uuid) -> Result<Value> {
        let path = format!("/organizations/{organization}/environments");
        let response = self.get(&path, organization_token).await?;
        response.require_success()?;
        response.json("Invalid environment response")
    }

    /// Revoke a session whose refresh token the CLI holds.
    async fn revoke(&self, refresh_token: &Secret) -> Result<()> {
        self.post(
            "/oauth/revoke",
            &[
                ("client_id", CLIENT_ID),
                ("token_type_hint", "refresh_token"),
                ("token", refresh_token.expose()),
            ],
        )
        .await?
        .require_success()
    }
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: Secret,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

impl DeviceAuthorization {
    /// The URL to open for approval: the complete one when offered, over HTTPS or loopback.
    fn verification_url(&self) -> Result<&str> {
        if self.device_code.expose().is_empty() || self.user_code.is_empty() {
            return Err(Error::auth("Missing device authorization code"));
        }
        let verification = self
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri);
        let url =
            url::Url::parse(verification).map_err(|_| Error::auth("Invalid verification URL"))?;
        let loopback =
            url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
        if url.scheme() != "https" && !loopback {
            return Err(Error::auth("Insecure verification URL"));
        }
        Ok(verification)
    }
}

/// RFC 8628 device grant errors; other codes are treated as failures.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum DeviceGrantError {
    AuthorizationPending,
    SlowDown,
    ExpiredToken,
    AccessDenied,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct DeviceGrantErrorBody {
    error: Option<DeviceGrantError>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Secret,
    refresh_token: Secret,
    token_type: String,
    expires_in: u64,
}

impl TokenResponse {
    fn into_login(self, user_id: Option<String>, email: Option<String>) -> Result<Login> {
        if self.access_token.expose().is_empty()
            || self.refresh_token.expose().is_empty()
            || self.expires_in == 0
            || !self.token_type.eq_ignore_ascii_case("bearer")
        {
            return Err(Error::auth("Invalid token response"));
        }
        Ok(Login {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: now().saturating_add(self.expires_in),
            user_id,
            email,
        })
    }
}

#[derive(Deserialize)]
struct OrganizationTokenResponse {
    access_token: Secret,
    issued_token_type: String,
    token_type: String,
    expires_in: u64,
}

/// The identity a fresh login is saved under.
#[derive(Deserialize)]
struct CurrentUser {
    id: String,
    email: Option<String>,
}

/// The organizations visible to a login, as the API lists them.
#[derive(Deserialize)]
struct OrganizationDiscovery {
    #[serde(default)]
    organizations: Value,
}

#[derive(Debug, Serialize)]
pub struct LoginOutcome {
    pub account: String,
    pub email: Option<String>,
    pub authenticated: bool,
    /// The login replaced a saved login of the same user under this name.
    pub replaced: bool,
    /// The profile saved for this login on a terminal, when one was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LogoutOutcome {
    pub account: String,
    pub removed: bool,
    pub local_only: bool,
}

#[derive(Debug, Serialize)]
pub struct ImportOutcome {
    pub account: String,
    pub imported: bool,
}

/// What `auth status` reports, without any secret.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum AuthStatus {
    /// `VOLTAGE_API_KEY` is in effect because no profile or account was selected.
    Ambient {
        source: &'static str,
        kind: AccountKind,
        configured: bool,
    },
    Saved {
        account: String,
        kind: AccountKind,
        email: Option<String>,
        expires_at: Option<u64>,
        expired: bool,
    },
}

/// `VOLTAGE_API_KEY` applies only when neither a profile nor an account was selected.
fn ambient_api_key(flags: &GlobalFlags) -> Option<Secret> {
    if flags.profile.is_some() || flags.account.is_some() {
        return None;
    }
    std::env::var("VOLTAGE_API_KEY").ok().map(Secret::new)
}

/// Where `VOLTAGE_API_KEY` and the organization came from, when this command sends that key.
/// A saved credential returns `None`.
pub(crate) fn environment_key_sources(
    flags: &GlobalFlags,
    sources: &ScopeSources,
) -> Option<CredentialSources> {
    ambient_api_key(flags).map(|_| CredentialSources {
        credential: Source::Environment,
        organization: sources.org,
    })
}

/// `VOLTAGE_API_KEY` is set, but a profile or account selection takes precedence over it.
pub(crate) fn ignores_ambient_api_key(flags: &GlobalFlags) -> bool {
    std::env::var_os("VOLTAGE_API_KEY").is_some() && ambient_api_key(flags).is_none()
}

/// The credential a command would use, chosen without reading a saved secret.
pub(crate) enum Selected<'a> {
    /// `VOLTAGE_API_KEY`.
    Ambient(Secret),
    Saved {
        name: String,
        account: &'a Account,
    },
}

/// A selection commands would reject, with the credential it named when there is one.
pub(crate) struct Rejected<'a> {
    pub(crate) problem: CredentialProblem,
    pub(crate) selected: Option<Selected<'a>>,
}

/// Choose the credential every command uses, and why commands would reject it. `resolve`
/// and `voltage context` share this, so inspection cannot drift from what commands do.
pub(crate) fn select<'a>(
    settings: &'a Settings,
    scope: &Scope,
    flags: &GlobalFlags,
) -> std::result::Result<Selected<'a>, Rejected<'a>> {
    if let Some(key) = ambient_api_key(flags) {
        // A blank key still shadows saved credentials, so commands fail rather than fall back.
        if key.expose().trim().is_empty() {
            return Err(Rejected {
                problem: CredentialProblem::EmptyApiKey,
                selected: Some(Selected::Ambient(key)),
            });
        }
        return Ok(Selected::Ambient(key));
    }
    let rejected = |problem| Rejected {
        problem,
        selected: None,
    };
    let name = settings.select_account(scope).map_err(rejected)?;
    let account = settings
        .config
        .accounts
        .get(&name)
        .ok_or(rejected(CredentialProblem::UnknownCredential))?;
    if account.bound_elsewhere(scope) {
        return Err(Rejected {
            problem: CredentialProblem::BoundElsewhere,
            selected: Some(Selected::Saved { name, account }),
        });
    }
    // A login refreshes only at its recorded origin. An invalid flag is not a mismatch here:
    // `resolve` reports it as an invalid URL.
    if account.kind == AccountKind::User
        && flags
            .auth_url
            .as_deref()
            .and_then(|raw| base_url(raw).ok())
            .is_some_and(|url| url != account.auth_url)
    {
        return Err(Rejected {
            problem: CredentialProblem::AuthUrlMismatch,
            selected: Some(Selected::Saved { name, account }),
        });
    }
    Ok(Selected::Saved { name, account })
}

pub fn status(settings: &Settings, scope: &Scope, flags: &GlobalFlags) -> Result<AuthStatus> {
    if let Some(key) = ambient_api_key(flags) {
        return Ok(AuthStatus::Ambient {
            source: "VOLTAGE_API_KEY",
            kind: AccountKind::ApiKey,
            configured: !key.expose().trim().is_empty(),
        });
    }
    let name = settings.account_name(scope)?;
    let account = settings.account(&name)?;
    let (email, expires_at) = match settings.read_credential(&name)? {
        Credential::ApiKey(_) => (None, None),
        Credential::Login(login) => (login.email, Some(login.expires_at)),
    };
    Ok(AuthStatus::Saved {
        account: name,
        kind: account.kind,
        email,
        expired: expires_at.is_some_and(|at| at <= now()),
        expires_at,
    })
}

/// Sign in with the OAuth device grant and save the login under an account name.
pub async fn login(
    settings: &mut Settings,
    flags: &LoginFlags,
    global: &GlobalFlags,
    submission: &SubmissionState,
) -> Result<LoginOutcome> {
    let terminal = global.terminal();
    let auth = AuthService::new(global.auth_url.as_deref().unwrap_or(AUTH_URL), terminal)?;
    // Only a login of the same user replaces a saved one. That user is known only after
    // the browser step, so a name that cannot be a login at this auth URL is refused first.
    // A saved login whose secret cannot be read cannot be shown to be the same user either.
    if let Some(name) = &global.account
        && let Some(saved) = settings.config.accounts.get(name)
    {
        if !is_login_at(saved, &auth.url) {
            return Err(name_in_use(name));
        }
        settings
            .read_credential(name)
            .map_err(|_| unreadable_name(name, saved.store))?;
    }
    let device = auth.device_authorization().await?;
    let verification = device.verification_url()?;
    terminal.important(format!(
        "Open {verification}\nConfirm this code: {}",
        device.user_code
    ));
    if !flags.no_browser && webbrowser::open(verification).is_err() {
        terminal.important("Could not open a browser; open the URL above manually.");
    }
    let token = auth.poll_device_grant(&device, submission).await?;
    let login = token.into_login(None, None)?;
    let session = Secret::new(login.refresh_token.expose().to_owned());
    let saved = save_login(settings, &auth, login, flags, global, submission).await;
    if saved.is_err() {
        // A local storage/discovery failure must not silently orphan a remote session.
        if auth.revoke(&session).await.is_err() {
            terminal.important(
                "Could not revoke the new CLI session after login failed. Use account global signout to invalidate its refresh session."
            );
        }
    }
    submission.session_settled();
    saved
}

/// Discover the identity behind a fresh login and save it; a local failure revokes the
/// new session so it is never silently orphaned on the server.
async fn save_login(
    settings: &mut Settings,
    auth: &AuthService,
    mut login: Login,
    flags: &LoginFlags,
    global: &GlobalFlags,
    submission: &SubmissionState,
) -> Result<LoginOutcome> {
    let user = auth.current_user(&login.access_token).await?;
    login.user_id = Some(user.id);
    login.email = user.email;
    let name = global
        .account
        .clone()
        .or_else(|| login.email.clone())
        .or_else(|| login.user_id.clone())
        .ok_or_else(|| Error::auth("User response has no account identity"))?;
    let email = login.email.clone();
    let user_id = login.user_id.clone();
    // Without `--credential-store`, a replaced login keeps its store and a new one uses the
    // keychain. `put_account` reads the account again under the lock; if another process
    // changed it in between, the replacement is a store change, which it handles.
    let store = flags.credential_store.unwrap_or_else(|| {
        settings
            .config
            .accounts
            .get(&name)
            .filter(|saved| is_login_at(saved, &auth.url))
            .map_or(CredentialStore::Keychain, |saved| saved.store)
    });
    let account = Account {
        store,
        kind: AccountKind::User,
        email: email.clone(),
        organization_id: None,
        environment_id: None,
        auth_url: auth.url.clone(),
    };
    let replaced = settings
        .put_account(
            &name,
            account,
            &Credential::Login(login),
            auth.terminal,
            |saved| is_login_at(saved, &auth.url),
            // A saved login without a user ID cannot be shown to be the same user.
            |credential| {
                matches!(credential, Credential::Login(old) if old.user_id.is_some() && old.user_id == user_id)
            },
        )
        .await?;
    // The new login is saved, so only the replaced session's revoke is still in flight.
    match &replaced {
        Some(_) => submission.session_changing(SessionChange::RevokeReplaced {
            account: name.clone(),
        }),
        None => submission.session_settled(),
    }
    if let Some(Credential::Login(old)) = &replaced {
        auth.terminal
            .notice(format!("Replaced the saved login for {name}."));
        if auth.revoke(&old.refresh_token).await.is_err() {
            auth.terminal.important(
                "Could not revoke the replaced CLI session. Use account global signout to invalidate its refresh session.",
            );
        }
    }
    Ok(LoginOutcome {
        account: name,
        email,
        authenticated: true,
        replaced: replaced.is_some(),
        profile: None,
    })
}

/// A saved browser login against the auth service at `url`, which a new login can replace.
fn is_login_at(account: &Account, url: &str) -> bool {
    account.kind == AccountKind::User && account.auth_url == url
}

fn saved_login(settings: &Settings, name: &str) -> Result<Login> {
    match settings.read_credential(name)? {
        Credential::Login(login) => Ok(login),
        Credential::ApiKey(_) => Err(Error::auth("Invalid saved credential")),
    }
}

/// A saved login the auth service no longer accepts, with the commands that replace it.
fn expired_login(account: &str, url: &str) -> Error {
    let account = shell_word(account);
    // `--auth-url` is never saved, so a login against another auth service names it again.
    let auth_url = if base_url(AUTH_URL).is_ok_and(|default| default == url) {
        String::new()
    } else {
        format!(" --auth-url {}", shell_word(url))
    };
    Error::auth(format!(
        "The saved login for {account} has expired or was revoked"
    ))
    .with_detail(ErrorDetail::Credential {
        credential_problem: CredentialRejection::LoginExpired,
    })
    // A login of the same user replaces the stale one, so nothing has to be removed first.
    .with_hint(format!(
        "Sign in again with `voltage login --account {account}{auth_url}`."
    ))
}

/// Resolves, refreshes, and exchanges one command's credential, and lists what it can see.
/// It holds what stays the same for the whole command. The scope is an argument to each call,
/// because a picker can fill in the organization between two calls.
#[derive(Clone, Copy)]
pub struct Resolver<'a> {
    pub settings: &'a Settings,
    pub flags: &'a GlobalFlags,
    /// Records a refresh in flight, so an interruption can report it.
    pub submission: &'a SubmissionState,
    /// Where `VOLTAGE_API_KEY` and the organization came from, when this command sends that
    /// key, from [`environment_key_sources`].
    pub credential_sources: Option<CredentialSources>,
}

impl Resolver<'_> {
    /// The saved credential for this command, refreshing an expiring login under the lock.
    pub async fn resolve(self, scope: &Scope) -> Result<Credential> {
        let Self {
            settings,
            flags,
            submission,
            ..
        } = self;
        let (name, account) = match select(settings, scope, flags)
            .map_err(|rejected| rejected.problem.error(scope, settings))?
        {
            Selected::Ambient(key) => return Ok(Credential::ApiKey(key)),
            Selected::Saved { name, account } => (name, account),
        };
        match account.kind {
            AccountKind::ApiKey => settings.read_credential(&name),
            AccountKind::User => {
                // `select` rejected a valid flag that differs from the saved URL.
                let url = base_url(flags.auth_url.as_deref().unwrap_or(&account.auth_url))?;
                // Credential writes replace the whole value atomically, so a fresh login is used
                // without the lock. Only a refresh, which spends the single-use refresh token, is
                // serialized.
                let login = saved_login(settings, &name)?;
                if login.expires_at > now() + REFRESH_MARGIN {
                    return Ok(Credential::Login(login));
                }
                let _lock = settings.lock(flags.terminal()).await?;
                // Another process may have refreshed while this one waited.
                let login = saved_login(settings, &name)?;
                if login.expires_at > now() + REFRESH_MARGIN {
                    return Ok(Credential::Login(login));
                }
                let auth = AuthService::new(&url, flags.terminal())?;
                submission.session_changing(SessionChange::Refresh {
                    account: name.clone(),
                });
                let refreshed = Credential::Login(auth.refresh(login, &name).await?);
                settings.write_credential(&name, account.store, &refreshed)?;
                submission.session_settled();
                Ok(refreshed)
            }
        }
    }

    /// The credential for organization API calls: the API key as is, or the login exchanged
    /// for a token scoped to `--org`.
    pub async fn resolve_organization(self, scope: &Scope) -> Result<ApiCredential> {
        let credential = self.resolve(scope).await?;
        self.exchange(scope, credential).await
    }

    /// The credential to send to the Voltage API for the scope's organization. An API key is sent
    /// unchanged. A browser login's access token is account-wide and the API does not accept it,
    /// so it is traded at the auth service (OAuth token exchange) for a short-lived token whose
    /// audience is the `--org` organization.
    pub async fn exchange(self, scope: &Scope, credential: Credential) -> Result<ApiCredential> {
        let login = match credential {
            Credential::ApiKey(key) => return Ok(ApiCredential::ApiKey(key)),
            Credential::Login(login) => login,
        };
        let token = self
            .organization_token(scope, &login.access_token, self.flags.terminal())
            .await?;
        Ok(ApiCredential::OrganizationToken(token))
    }

    /// A login's access token traded for a token whose audience is the scope's organization.
    /// `terminal` shows the exchange's progress; `wallets list --all-orgs` runs several side
    /// by side, so it passes a quiet one and shows one progress line for all of them.
    pub async fn organization_token(
        self,
        scope: &Scope,
        access_token: &Secret,
        terminal: Terminal,
    ) -> Result<Secret> {
        let organization = scope.require_org()?;
        let name = self.settings.account_name(scope)?;
        self.account_auth(scope, terminal)?
            .exchange(access_token, organization, &name)
            .await
    }

    /// List organizations visible to the login, or environments in the selected organization.
    pub async fn discover(self, scope: &Scope, what: Discovery) -> Result<Value> {
        // An API key cannot list organizations or environments; say which key was used.
        let requires_login = || {
            let command = match what {
                Discovery::Organizations => "organizations list",
                Discovery::Environments => "environments list",
            };
            if ambient_api_key(self.flags).is_some() {
                return Error::auth(format!(
                    "{command} needs a browser login, but this command used VOLTAGE_API_KEY from the environment"
                ))
                .with_hint(
                    "Pass --account NAME to use a saved browser login (`voltage login` creates one), or unset VOLTAGE_API_KEY.",
                );
            }
            let used = self.settings.account_name(scope).unwrap_or_default();
            Error::auth(format!(
                "{command} needs a browser login, but the selected credential {used} is an API key"
            ))
            .with_hint(
                "Select a browser login with --account NAME, or sign in with `voltage login`.",
            )
        };
        let token = match what {
            Discovery::Organizations => match self.resolve(scope).await? {
                Credential::Login(login) => login.access_token,
                Credential::ApiKey(_) => return Err(requires_login()),
            },
            Discovery::Environments => match self.resolve_organization(scope).await? {
                ApiCredential::OrganizationToken(token) => token,
                ApiCredential::ApiKey(_) => return Err(requires_login()),
            },
        };
        self.list(scope, what, &token).await
    }

    /// One discovery listing with a token already in hand: the login's access token for
    /// organizations, or an organization token for environments.
    pub async fn list(self, scope: &Scope, what: Discovery, token: &Secret) -> Result<Value> {
        let auth = self.account_auth(scope, self.flags.terminal())?;
        match what {
            Discovery::Organizations => auth.organizations(token).await,
            Discovery::Environments => auth.environments(token, scope.require_org()?).await,
        }
    }

    /// The auth service of the account the scope selects, showing progress on `terminal`.
    fn account_auth(self, scope: &Scope, terminal: Terminal) -> Result<AuthService> {
        let name = self.settings.account_name(scope)?;
        AuthService::new(&self.settings.account(&name)?.auth_url, terminal)
    }
}

pub async fn logout(
    settings: &mut Settings,
    scope: &Scope,
    local: bool,
    terminal: Terminal,
    submission: &SubmissionState,
) -> Result<LogoutOutcome> {
    // The lock spans the revoke on purpose. Released, a concurrent refresh could rotate the
    // token in between, and logout would revoke the spent token while the new session stays
    // live. Logout is rare, and a fresh login is used without the lock, so only refreshes and
    // configuration writes wait behind it, for at most AUTH_TIMEOUT.
    let _lock = settings.lock(terminal).await?;
    settings.reload()?;
    let name = settings.account_name(scope)?;
    let account = settings.account(&name)?;
    if account.kind == AccountKind::User && !local {
        let Credential::Login(login) = settings.read_credential(&name)? else {
            return Err(Error::auth(
                "Missing refresh token; use --local to remove local credentials",
            ));
        };
        let auth = AuthService::new(&account.auth_url, terminal)?;
        submission.session_changing(SessionChange::Logout {
            account: name.clone(),
        });
        auth.revoke(&login.refresh_token).await?;
    }
    // `--local` is how a person removes a login whose secret cannot be removed, so a failed
    // removal there is only a warning.
    if local {
        settings.discard_credential(&name, terminal)?;
    } else {
        settings.delete_credential(&name)?;
    }
    settings.config.accounts.remove(&name);
    settings.save()?;
    submission.session_settled();
    Ok(LogoutOutcome {
        account: name,
        removed: true,
        local_only: local,
    })
}

/// Save an environment API key with the organization and environment it is bound to.
pub async fn import_key(
    settings: &mut Settings,
    scope: &Scope,
    flags: &ImportKeyFlags,
    global: &GlobalFlags,
) -> Result<ImportOutcome> {
    let name = global
        .account
        .clone()
        .ok_or_else(|| Error::usage("--account is required to name the saved API key"))?;
    let organization = scope.require_org()?;
    let environment = match scope.envs.as_slice() {
        [env] => *env,
        _ => return Err(Error::usage("Exactly one --env is required")),
    };
    let key = if flags.stdin {
        read_secret(&InputSource::Stdin).await?
    } else {
        let terminal = global.terminal();
        if !terminal.can_prompt() {
            return Err(Error::usage(
                "Use --stdin to import an API key when input is disabled or noninteractive",
            ));
        }
        terminal.read_hidden("Environment API key: ").await?
    };
    if key.expose().trim().is_empty() {
        return Err(Error::usage("API key cannot be empty"));
    }
    let account = Account {
        store: flags.credential_store,
        kind: AccountKind::ApiKey,
        email: None,
        organization_id: Some(organization),
        environment_id: Some(environment),
        auth_url: AUTH_URL.into(),
    };
    settings
        .add_account(&name, account, &Credential::ApiKey(key), global.terminal())
        .await?;
    Ok(ImportOutcome {
        account: name,
        imported: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_urls_require_https_except_on_loopback() {
        assert_eq!(
            base_url("https://auth.example.test/api/v1/").unwrap(),
            "https://auth.example.test/api/v1"
        );
        assert!(base_url("http://127.0.0.1:8081/api/v1").is_ok());
        assert!(base_url("http://auth.example.test").is_err());
        assert!(base_url("https://user:pw@auth.example.test").is_err());
        assert!(base_url("https://auth.example.test/?x=1").is_err());
    }

    #[test]
    fn token_responses_validate_before_becoming_logins() {
        let token: TokenResponse = serde_json::from_str(
            r#"{"access_token":"a","refresh_token":"r","token_type":"Bearer","expires_in":600}"#,
        )
        .unwrap();
        let login = token.into_login(Some("user".into()), None).unwrap();
        assert!(login.expires_at > now());
        assert_eq!(login.access_token.expose(), "a");
        let wrong_type: TokenResponse = serde_json::from_str(
            r#"{"access_token":"a","refresh_token":"r","token_type":"MAC","expires_in":600}"#,
        )
        .unwrap();
        assert!(wrong_type.into_login(None, None).is_err());
    }

    #[test]
    fn verification_urls_must_be_https_or_loopback_with_codes_present() {
        let device = |complete: Option<&str>, uri: &str, code: &str| DeviceAuthorization {
            device_code: Secret::new(code.into()),
            user_code: "aB1d-eF2z".into(),
            verification_uri: uri.into(),
            verification_uri_complete: complete.map(String::from),
            expires_in: 600,
            interval: Some(5),
        };
        assert_eq!(
            device(
                Some("https://app.example.test/cli?code=x"),
                "https://app.example.test/cli",
                "d"
            )
            .verification_url()
            .unwrap(),
            "https://app.example.test/cli?code=x"
        );
        assert_eq!(
            device(None, "http://localhost:3210/cli", "d")
                .verification_url()
                .unwrap(),
            "http://localhost:3210/cli"
        );
        assert_eq!(
            device(None, "http://app.example.test/cli", "d")
                .verification_url()
                .unwrap_err()
                .message,
            "Insecure verification URL"
        );
        assert_eq!(
            device(None, "not a url", "d")
                .verification_url()
                .unwrap_err()
                .message,
            "Invalid verification URL"
        );
        assert_eq!(
            device(None, "https://app.example.test/cli", "")
                .verification_url()
                .unwrap_err()
                .message,
            "Missing device authorization code"
        );
    }

    #[test]
    fn device_grant_errors_keep_unknown_codes_distinct() {
        let body: DeviceGrantErrorBody = serde_json::from_str(r#"{"error":"slow_down"}"#).unwrap();
        assert_eq!(body.error, Some(DeviceGrantError::SlowDown));
        let body: DeviceGrantErrorBody =
            serde_json::from_str(r#"{"error":"server_error"}"#).unwrap();
        assert_eq!(body.error, Some(DeviceGrantError::Other));
    }

    #[test]
    fn expired_logins_name_commands_that_can_be_pasted() {
        let hint = |account, url| expired_login(account, url).hint.unwrap();
        let default = base_url(AUTH_URL).unwrap();
        // Agents detect a dead login by its problem, not by parsing the message.
        assert!(matches!(
            expired_login("me", &default).detail,
            Some(ErrorDetail::Credential {
                credential_problem: CredentialRejection::LoginExpired
            })
        ));
        assert_eq!(
            hint("me@example.com", &default),
            "Sign in again with `voltage login --account me@example.com`."
        );
        // `--auth-url` is never saved, so another auth service is named again.
        assert!(hint("me", "https://auth.staging.example.test").ends_with(
            "`voltage login --account me --auth-url https://auth.staging.example.test`."
        ));
        assert!(hint("my team", &default).contains("--account 'my team'`"));
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word(""), "''");
        assert_eq!(shell_word("$(id)"), "'$(id)'");
    }

    #[test]
    fn failed_responses_report_only_identifier_error_codes() {
        let response = AuthResponse {
            status: 400,
            body: Zeroizing::new(br#"{"error":"invalid_grant"}"#.to_vec()),
        };
        assert_eq!(
            response.require_success().unwrap_err().message,
            "Authentication failed (HTTP 400, invalid_grant)"
        );
        let response = AuthResponse {
            status: 500,
            body: Zeroizing::new(br#"{"error":"<script>alert(1)</script>"}"#.to_vec()),
        };
        assert_eq!(
            response.require_success().unwrap_err().message,
            "Authentication failed (HTTP 500, authentication_failed)"
        );
    }

    /// The auth contract snapshot; every endpoint and field the CLI relies on must be in it.
    fn auth_contract() -> Value {
        serde_json::from_str(include_str!("../api/auth-openapi.json")).unwrap()
    }

    fn schema<'a>(spec: &'a Value, reference: &'a Value) -> &'a Value {
        match reference.get("$ref").and_then(Value::as_str) {
            Some(path) => &spec["components"]["schemas"][path.rsplit('/').next().unwrap()],
            None => reference,
        }
    }

    fn response_properties<'a>(spec: &'a Value, path: &str, method: &str) -> Vec<&'a Value> {
        let body = &spec["paths"][path][method]["responses"]["200"]["content"]["application/json"]
            ["schema"];
        let body = schema(spec, body);
        match body["oneOf"].as_array() {
            Some(alternatives) => alternatives.iter().map(|alt| schema(spec, alt)).collect(),
            None => vec![body],
        }
    }

    fn has_property(schemas: &[&Value], name: &str) -> bool {
        schemas
            .iter()
            .any(|schema| schema["properties"].get(name).is_some())
    }

    #[test]
    fn the_cli_uses_only_endpoints_and_fields_in_the_auth_contract() {
        let spec = auth_contract();
        let form = |path: &str, fields: &[&str]| {
            let request = &spec["paths"][path]["post"]["requestBody"]["content"]["application/x-www-form-urlencoded"]
                ["schema"];
            let request = schema(&spec, request);
            for field in fields {
                assert!(
                    request["properties"].get(field).is_some(),
                    "{path} does not accept {field}"
                );
            }
        };
        form("/api/v1/oauth/device_authorization", &["client_id"]);
        form(
            "/api/v1/oauth/token",
            &[
                "grant_type",
                "client_id",
                "device_code",
                "refresh_token",
                "subject_token",
                "subject_token_type",
                "audience",
            ],
        );
        form(
            "/api/v1/oauth/revoke",
            &["client_id", "token", "token_type_hint"],
        );
        let device = response_properties(&spec, "/api/v1/oauth/device_authorization", "post");
        for field in [
            "device_code",
            "user_code",
            "verification_uri",
            "verification_uri_complete",
            "expires_in",
            "interval",
        ] {
            assert!(
                has_property(&device, field),
                "device authorization lacks {field}"
            );
        }
        let token = response_properties(&spec, "/api/v1/oauth/token", "post");
        for field in [
            "access_token",
            "refresh_token",
            "token_type",
            "expires_in",
            "issued_token_type",
        ] {
            assert!(has_property(&token, field), "token response lacks {field}");
        }
        let user = response_properties(&spec, "/api/v1/users/current", "get");
        for field in ["id", "email", "organizations"] {
            assert!(has_property(&user, field), "current user lacks {field}");
        }
        assert!(
            spec["paths"]["/api/v1/organizations/{organization_id}/environments"]["get"]
                .is_object()
        );
        let errors = &spec["components"]["schemas"]["OAuthError"]["properties"];
        assert!(errors.get("error").is_some());
    }

    #[test]
    fn response_chunks_move_into_zeroized_memory() {
        let mut body = Zeroizing::new(Vec::new());
        absorb_chunk(&mut body, bytes::Bytes::from(b"tok".to_vec()));
        absorb_chunk(&mut body, bytes::Bytes::from_static(b"en"));
        assert_eq!(&body[..], b"token");
    }

    #[tokio::test]
    async fn an_auth_timeout_names_its_fixed_limit_not_timeout_flag() {
        // The listener takes the connection but never answers. The client's short limit
        // stands in for AUTH_TIMEOUT, which the message must still name.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let request = client(Duration::from_millis(100))
            .unwrap()
            .get(format!("http://{}/", silent.local_addr().unwrap()));
        let Err(error) = auth_request(request, Terminal::new(true, true)).await else {
            panic!("a silent auth service must fail");
        };
        assert_eq!(error.kind, crate::error::ErrorKind::Transport);
        assert_eq!(
            error.message,
            "Authentication request failed: no response within 30 seconds; no token request was automatically retried"
        );
        assert_eq!(
            error.hint.as_deref(),
            Some(
                "Retry. The auth service has a fixed 30-second limit, which --timeout does not change."
            )
        );
        drop(silent);
    }

    #[tokio::test]
    async fn oversized_auth_responses_are_rejected() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_raw(vec![b'x'; 64], "text/plain"),
            )
            .mount(&server)
            .await;
        let response = client(AUTH_TIMEOUT)
            .unwrap()
            .get(server.uri())
            .send()
            .await
            .unwrap();
        let error = bounded_body(response, 16).await.unwrap_err();
        assert_eq!(error.message, "Authentication response exceeds 1 MiB");
        let response = client(AUTH_TIMEOUT)
            .unwrap()
            .get(server.uri())
            .send()
            .await
            .unwrap();
        assert_eq!(bounded_body(response, 64).await.unwrap().len(), 64);
    }
}
