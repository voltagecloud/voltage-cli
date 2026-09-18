//! Browser device login, token refresh, organization token exchange, and credential selection.
//!
//! Token responses deserialize straight into `Secret` fields and the response buffer is
//! zeroized, so plaintext tokens never sit in a general-purpose `Value`. Token mutations are
//! never retried; only device polling backs off.

use crate::{
    Error, Result,
    cli::{GlobalFlags, ImportKeyFlags, LoginFlags},
    config::{
        AUTH_URL, Account, AccountKind, Credential, InputSource, Login, Scope, Settings,
        read_secret,
    },
    secret::Secret,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    io::IsTerminal,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::{Zeroize, Zeroizing};

const CLIENT_ID: &str = "voltage-cli";
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const TOKEN_EXCHANGE_GRANT: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
/// Access tokens this close to expiry are refreshed before use.
const REFRESH_MARGIN: u64 = 30;
/// Device approval waits are bounded regardless of the server's `expires_in`.
const MAX_LOGIN_WAIT: u64 = 600;
const MIN_POLL_INTERVAL: u64 = 5;
const MAX_POLL_INTERVAL: u64 = 600;
const SLOW_DOWN_STEP: u64 = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
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

    /// Fail with the server's error code when it is a plain identifier, never its free text.
    fn require_success(&self) -> Result<()> {
        if self.is_success() {
            return Ok(());
        }
        let error = serde_json::from_slice::<OAuthErrorBody>(&self.body)
            .ok()
            .and_then(|body| body.error)
            .filter(|code| {
                code.len() < 80 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
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

async fn auth_request(request: reqwest::RequestBuilder) -> Result<AuthResponse> {
    let response = request.send().await.map_err(|_| {
        Error::transport(
            "Authentication request failed; no token request was automatically retried",
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
) -> Result<LoginOutcome> {
    let url = base_url(global.auth_url.as_deref().unwrap_or(AUTH_URL))?;
    if let Some(name) = &global.account
        && settings.config.accounts.contains_key(name)
    {
        return Err(Error::usage(
            "Credential already exists; log out first or choose another --account",
        ));
    }
    let client = client(AUTH_TIMEOUT)?;
    let response = auth_request(
        client
            .post(format!("{url}/oauth/device_authorization"))
            .form(&[("client_id", CLIENT_ID)]),
    )
    .await?;
    response.require_success()?;
    let device: DeviceAuthorization = response.json("Invalid device authorization response")?;
    let verification = device.verification_url()?;
    eprintln!(
        "Open {verification}\nConfirm this code: {}",
        device.user_code
    );
    if !flags.no_browser && webbrowser::open(verification).is_err() {
        eprintln!("Could not open a browser; open the URL above manually.");
    }
    let token = poll_device_grant(&client, &url, &device).await?;
    let login = token.into_login(None, None)?;
    let session = Secret::new(login.refresh_token.expose().to_owned());
    let saved = save_login(settings, login, flags, global, &client, &url).await;
    if saved.is_err() {
        // A local storage/discovery failure must not silently orphan a remote session.
        if revoke(&client, &url, &session).await.is_err() {
            eprintln!(
                "Could not revoke the new CLI session after login failed. Use account global signout to invalidate its refresh session."
            );
        }
    }
    saved
}

/// Poll the token endpoint at the server's interval until approval, denial, or expiry.
async fn poll_device_grant(
    client: &reqwest::Client,
    url: &str,
    device: &DeviceAuthorization,
) -> Result<TokenResponse> {
    let expires = device.expires_in.min(MAX_LOGIN_WAIT);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(expires);
    let mut interval = device
        .interval
        .unwrap_or(MIN_POLL_INTERVAL)
        .clamp(MIN_POLL_INTERVAL, MAX_POLL_INTERVAL);
    loop {
        if tokio::time::Instant::now() + Duration::from_secs(interval) >= deadline {
            return Err(Error::timeout("Login expired; run voltage login again"));
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let response = tokio::time::timeout_at(
            deadline,
            auth_request(client.post(format!("{url}/oauth/token")).form(&[
                ("client_id", CLIENT_ID),
                ("grant_type", DEVICE_CODE_GRANT),
                ("device_code", device.device_code.expose()),
            ])),
        )
        .await
        .map_err(|_| Error::timeout("Login expired"))??;
        if response.is_success() {
            return response.json("Invalid token response");
        }
        let error = response
            .json::<DeviceGrantErrorBody>("Invalid authentication response")
            .ok()
            .and_then(|body| body.error);
        match error {
            Some(DeviceGrantError::AuthorizationPending) => {}
            Some(DeviceGrantError::SlowDown) => interval = interval.saturating_add(SLOW_DOWN_STEP),
            Some(DeviceGrantError::ExpiredToken) => return Err(Error::timeout("Login expired")),
            Some(DeviceGrantError::AccessDenied) => return Err(Error::auth("Login was denied")),
            Some(DeviceGrantError::Other) | None => response.require_success()?,
        }
    }
}

/// Discover the identity behind a fresh login and save it; a local failure revokes the
/// new session so it is never silently orphaned on the server.
async fn save_login(
    settings: &mut Settings,
    mut login: Login,
    flags: &LoginFlags,
    global: &GlobalFlags,
    client: &reqwest::Client,
    url: &str,
) -> Result<LoginOutcome> {
    async {
        let response = auth_request(
            client
                .get(format!("{url}/users/current"))
                .bearer_auth(login.access_token.expose()),
        )
        .await?;
        response.require_success()?;
        let user: CurrentUser = response.json("Invalid user response")?;
        login.user_id = Some(user.id);
        login.email = user.email;
        let name = global
            .account
            .clone()
            .or_else(|| login.email.clone())
            .or_else(|| login.user_id.clone())
            .ok_or_else(|| Error::auth("User response has no account identity"))?;
        let _lock = settings.lock().await?;
        settings.reload()?;
        if settings.config.accounts.contains_key(&name) {
            return Err(Error::usage(format!(
                "Credential {name} already exists; log it out first or use a different --account name"
            )));
        }
        let email = login.email.clone();
        settings.write_credential(&name, flags.credential_store, &Credential::Login(login))?;
        settings.config.accounts.insert(
            name.clone(),
            Account {
                store: flags.credential_store,
                kind: AccountKind::User,
                email: email.clone(),
                organization_id: None,
                environment_id: None,
                auth_url: url.to_owned(),
            },
        );
        if let Err(error) = settings.save() {
            // Do not leave a stored secret that no account entry can reach.
            let _ = settings.delete_credential(&name);
            settings.config.accounts.remove(&name);
            return Err(error);
        }
        Ok(LoginOutcome {
            account: name,
            email,
            authenticated: true,
        })
    }
    .await
}

/// Revoke a session whose refresh token the CLI holds.
async fn revoke(client: &reqwest::Client, url: &str, refresh_token: &Secret) -> Result<()> {
    auth_request(client.post(format!("{url}/oauth/revoke")).form(&[
        ("client_id", CLIENT_ID),
        ("token_type_hint", "refresh_token"),
        ("token", refresh_token.expose()),
    ]))
    .await?
    .require_success()
}

/// The saved credential for this command, refreshing an expiring login under the lock.
pub async fn resolve(
    settings: &Settings,
    scope: &Scope,
    flags: &GlobalFlags,
) -> Result<Credential> {
    if let Some(key) = ambient_api_key(flags) {
        if key.expose().trim().is_empty() {
            return Err(Error::auth("VOLTAGE_API_KEY is empty"));
        }
        return Ok(Credential::ApiKey(key));
    }
    let name = settings.account_name(scope)?;
    let account = settings.account(&name)?;
    match account.kind {
        AccountKind::ApiKey => {
            let organization_mismatch = scope
                .org
                .zip(account.organization_id)
                .is_some_and(|(selected, bound)| selected != bound);
            let environment_mismatch = account
                .environment_id
                .is_some_and(|bound| scope.envs.iter().any(|env| *env != bound));
            if organization_mismatch || environment_mismatch {
                return Err(Error::usage(
                    "API key is bound to a different organization or environment; select a matching credential",
                ));
            }
            settings.read_credential(&name)
        }
        AccountKind::User => {
            let url = base_url(flags.auth_url.as_deref().unwrap_or(&account.auth_url))?;
            if url != account.auth_url {
                return Err(Error::usage(
                    "Saved credentials cannot be refreshed at a different auth URL",
                ));
            }
            let _lock = settings.lock().await?;
            let Credential::Login(login) = settings.read_credential(&name)? else {
                return Err(Error::auth("Invalid saved credential"));
            };
            if login.expires_at > now() + REFRESH_MARGIN {
                return Ok(Credential::Login(login));
            }
            let refreshed = Credential::Login(refresh(&url, login).await?);
            settings.write_credential(&name, account.store, &refreshed)?;
            Ok(refreshed)
        }
    }
}

/// Rotate an expiring login; the refresh token is single use, so the result is saved
/// before any caller sees it.
async fn refresh(url: &str, login: Login) -> Result<Login> {
    let response = auth_request(
        client(AUTH_TIMEOUT)?
            .post(format!("{url}/oauth/token"))
            .form(&[
                ("client_id", CLIENT_ID),
                ("grant_type", "refresh_token"),
                ("refresh_token", login.refresh_token.expose()),
            ]),
    )
    .await?;
    response.require_success()?;
    let token: TokenResponse = response.json("Invalid token response")?;
    token.into_login(login.user_id, login.email)
}

/// The credential for organization API calls: the API key as is, or the login exchanged
/// for a token scoped to `--org`.
pub async fn resolve_organization(
    settings: &Settings,
    scope: &Scope,
    flags: &GlobalFlags,
) -> Result<ApiCredential> {
    let login = match resolve(settings, scope, flags).await? {
        Credential::ApiKey(key) => return Ok(ApiCredential::ApiKey(key)),
        Credential::Login(login) => login,
    };
    let organization = scope.require_org()?;
    let name = settings.account_name(scope)?;
    let url = base_url(&settings.account(&name)?.auth_url)?;
    let response = auth_request(
        client(AUTH_TIMEOUT)?
            .post(format!("{url}/oauth/token"))
            .form(&[
                ("grant_type", TOKEN_EXCHANGE_GRANT),
                ("subject_token", login.access_token.expose()),
                ("subject_token_type", ACCESS_TOKEN_TYPE),
                ("audience", &organization.to_string()),
            ]),
    )
    .await?;
    response.require_success()?;
    let token: OrganizationTokenResponse = response.json("Invalid organization token response")?;
    if token.access_token.expose().is_empty()
        || token.issued_token_type != ACCESS_TOKEN_TYPE
        || !token.token_type.eq_ignore_ascii_case("bearer")
        || token.expires_in == 0
    {
        return Err(Error::auth("Invalid organization token response"));
    }
    Ok(ApiCredential::OrganizationToken(token.access_token))
}

pub async fn logout(settings: &mut Settings, scope: &Scope, local: bool) -> Result<LogoutOutcome> {
    let _lock = settings.lock().await?;
    settings.reload()?;
    let name = settings.account_name(scope)?;
    let account = settings.account(&name)?;
    if account.kind == AccountKind::User && !local {
        let Credential::Login(login) = settings.read_credential(&name)? else {
            return Err(Error::auth(
                "Missing refresh token; use --local to remove local credentials",
            ));
        };
        let url = base_url(&account.auth_url)?;
        revoke(&client(AUTH_TIMEOUT)?, &url, &login.refresh_token).await?;
    }
    settings.delete_credential(&name)?;
    settings.config.accounts.remove(&name);
    settings.save()?;
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
        read_secret(&InputSource::Stdin)?
    } else {
        if !std::io::stdin().is_terminal() {
            return Err(Error::usage(
                "Use --stdin to import an API key noninteractively",
            ));
        }
        Secret::new(
            rpassword::prompt_password("Environment API key: ")
                .map_err(|error| Error::transport(error.to_string()))?,
        )
    };
    if key.expose().trim().is_empty() {
        return Err(Error::usage("API key cannot be empty"));
    }
    let _lock = settings.lock().await?;
    settings.reload()?;
    if settings.config.accounts.contains_key(&name) {
        return Err(Error::usage("Credential already exists"));
    }
    settings.write_credential(&name, flags.credential_store, &Credential::ApiKey(key))?;
    settings.config.accounts.insert(
        name.clone(),
        Account {
            store: flags.credential_store,
            kind: AccountKind::ApiKey,
            email: None,
            organization_id: Some(organization),
            environment_id: Some(environment),
            auth_url: AUTH_URL.into(),
        },
    );
    settings.save()?;
    Ok(ImportOutcome {
        account: name,
        imported: true,
    })
}

/// List organizations visible to the login, or environments in the selected organization.
pub async fn discover(
    settings: &Settings,
    scope: &Scope,
    flags: &GlobalFlags,
    what: Discovery,
) -> Result<Value> {
    let requires_login = || Error::auth("Organization/environment discovery requires a user login");
    let token = match what {
        Discovery::Organizations => match resolve(settings, scope, flags).await? {
            Credential::Login(login) => login.access_token,
            Credential::ApiKey(_) => return Err(requires_login()),
        },
        Discovery::Environments => match resolve_organization(settings, scope, flags).await? {
            ApiCredential::OrganizationToken(token) => token,
            ApiCredential::ApiKey(_) => return Err(requires_login()),
        },
    };
    let name = settings.account_name(scope)?;
    let url = base_url(&settings.account(&name)?.auth_url)?;
    let path = match what {
        Discovery::Organizations => "/users/current".to_owned(),
        Discovery::Environments => {
            format!("/organizations/{}/environments", scope.require_org()?)
        }
    };
    let response = auth_request(
        client(AUTH_TIMEOUT)?
            .get(format!("{url}{path}"))
            .bearer_auth(token.expose()),
    )
    .await?;
    response.require_success()?;
    match what {
        Discovery::Organizations => Ok(response
            .json::<OrganizationDiscovery>("Invalid user response")?
            .organizations),
        Discovery::Environments => response.json("Invalid environment response"),
    }
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
