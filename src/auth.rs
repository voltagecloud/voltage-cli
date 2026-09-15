use crate::{
    Error, Result, cli,
    config::{AUTH_URL, Account, Credential, Scope, Settings},
};
use clap::ArgMatches;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
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
pub fn client(timeout: u64) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(timeout))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("voltage-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| Error::io("Cannot initialize HTTP client"))
}
async fn auth_request(request: reqwest::RequestBuilder) -> Result<(u16, Value)> {
    let response = request.send().await.map_err(|_| {
        Error::io("Authentication request failed; no token request was automatically retried")
    })?;
    let status = response.status().as_u16();
    let bytes = response
        .bytes()
        .await
        .map_err(|_| Error::io("Unable to read authentication response"))?;
    // Ingress rate limits can return plain text rather than an OAuth response.
    // Device polling still needs to back off; token mutations are never retried.
    let body = if status == 429 {
        json!({"error": "slow_down"})
    } else if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|_| Error::auth("Invalid authentication response"))?
    };
    Ok((status, body))
}
fn require_success(status: u16, body: &Value) -> Result<()> {
    if (200..300).contains(&status) {
        Ok(())
    } else {
        let error = body
            .get("error")
            .and_then(Value::as_str)
            .filter(|s| s.len() < 80 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or("authentication_failed");
        Err(Error::auth(format!(
            "Authentication failed (HTTP {status}, {error})"
        )))
    }
}
#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    token_type: String,
    expires_in: u64,
}
fn token_credential(body: &Value) -> Result<Credential> {
    let token: TokenResponse =
        serde_json::from_value(body.clone()).map_err(|_| Error::auth("Invalid token response"))?;
    if token.access_token.is_empty()
        || token.refresh_token.is_empty()
        || token.expires_in == 0
        || !token.token_type.eq_ignore_ascii_case("bearer")
    {
        return Err(Error::auth("Invalid token response"));
    }
    Ok(Credential {
        access_token: Some(token.access_token),
        refresh_token: Some(token.refresh_token),
        expires_at: Some(now().saturating_add(token.expires_in)),
        api_key: None,
        user_id: None,
        email: None,
    })
}
pub async fn login(settings: &mut Settings, m: &ArgMatches) -> Result<Value> {
    let url = base_url(&cli::value(m, "auth-url").unwrap_or_else(|| AUTH_URL.into()))?;
    if let Some(name) = cli::value(m, "account")
        && settings.config.accounts.contains_key(&name)
    {
        return Err(Error::usage(
            "Credential already exists; log out first or choose another --account",
        ));
    }
    let client = client(30)?;
    let (status, device) = auth_request(
        client
            .post(format!("{url}/oauth/device_authorization"))
            .form(&[("client_id", "voltage-cli")]),
    )
    .await?;
    require_success(status, &device)?;
    let device: DeviceAuthorization = serde_json::from_value(device)
        .map_err(|_| Error::auth("Invalid device authorization response"))?;
    if device.device_code.is_empty() || device.user_code.is_empty() {
        return Err(Error::auth("Missing device authorization code"));
    }
    let code = device.device_code.as_str();
    let user_code = device.user_code.as_str();
    let verification = device
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&device.verification_uri);
    let verification_url =
        url::Url::parse(verification).map_err(|_| Error::auth("Invalid verification URL"))?;
    if verification_url.scheme() != "https"
        && !(verification_url.scheme() == "http"
            && matches!(verification_url.host_str(), Some("localhost" | "127.0.0.1")))
    {
        return Err(Error::auth("Insecure verification URL"));
    }
    eprintln!("Open {verification}\nConfirm this code: {user_code}");
    if !cli::enabled(m, "no-browser") && webbrowser::open(verification).is_err() {
        eprintln!("Could not open a browser; open the URL above manually.");
    }
    let expires = device.expires_in.min(600);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(expires);
    let mut interval = device.interval.unwrap_or(5).clamp(5, 600);
    let mut credential = loop {
        if tokio::time::Instant::now() + Duration::from_secs(interval) >= deadline {
            return Err(Error::new(5, "Login expired; run voltage login again"));
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let result = tokio::time::timeout_at(
            deadline,
            auth_request(client.post(format!("{url}/oauth/token")).form(&[
                ("client_id", "voltage-cli"),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", code),
            ])),
        )
        .await
        .map_err(|_| Error::new(5, "Login expired"))??;
        let (status, body) = result;
        if (200..300).contains(&status) {
            break token_credential(&body)?;
        }
        match body["error"].as_str() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = interval.saturating_add(5),
            Some("expired_token") => return Err(Error::new(5, "Login expired")),
            Some("access_denied") => return Err(Error::auth("Login was denied")),
            _ => require_success(status, &body)?,
        }
    };
    let saved = async {
    let (status, user) = auth_request(
        client
            .get(format!("{url}/users/current"))
            .bearer_auth(credential.access_token.as_ref().unwrap()),
    )
    .await?;
    require_success(status, &user)?;
    credential.user_id = user["id"]
        .as_str()
        .or_else(|| user["user_id"].as_str())
        .map(String::from);
    credential.email = user["email"].as_str().map(String::from);
    let name = cli::value(m, "account")
        .or_else(|| credential.email.clone())
        .or_else(|| credential.user_id.clone())
        .ok_or_else(|| Error::auth("User response has no account identity"))?;
    let store = cli::value(m, "credential-store").unwrap_or_else(|| "keychain".into());
    let _lock = settings.lock().await?;
    settings.config = Settings::load(m)?.config;
    if settings.config.accounts.contains_key(&name) {
        return Err(Error::usage(format!(
            "Credential {name} already exists; log it out first or use a different --account name"
        )));
    }
    settings.write_credential(&name, &store, &credential)?;
    settings.config.accounts.insert(
        name.clone(),
        Account {
            store,
            kind: "user".into(),
            email: credential.email.clone(),
            organization_id: None,
            environment_id: None,
            auth_url: url.clone(),
        },
    );
    settings.save()?;
    Ok(json!({"account":name,"email":credential.email,"authenticated":true}))
    }.await;
    if saved.is_err() {
        // A local storage/discovery failure must not silently orphan a remote session.
        let revoked = auth_request(client.post(format!("{url}/oauth/revoke")).form(&[
            ("client_id", "voltage-cli"),
            (
                "token",
                credential.refresh_token.as_deref().unwrap_or_default(),
            ),
        ]))
        .await
        .and_then(|(status, body)| require_success(status, &body));
        if revoked.is_err() {
            eprintln!(
                "Could not revoke the new CLI session after login failed. Use account global signout to invalidate its refresh session."
            );
        }
    }
    saved
}

pub async fn resolve(settings: &Settings, scope: &Scope, m: &ArgMatches) -> Result<Credential> {
    if cli::value(m, "profile").is_none()
        && cli::value(m, "account").is_none()
        && let Ok(key) = std::env::var("VOLTAGE_API_KEY")
    {
        if key.trim().is_empty() {
            return Err(Error::auth("VOLTAGE_API_KEY is empty"));
        }
        return Ok(Credential {
            api_key: Some(key),
            access_token: None,
            refresh_token: None,
            expires_at: None,
            user_id: None,
            email: None,
        });
    }
    let name = settings.account_name(scope)?;
    let account = &settings.config.accounts[&name];
    if account.kind == "api_key" {
        if scope
            .org
            .as_ref()
            .zip(account.organization_id.as_ref())
            .is_some_and(|(a, b)| a != b)
            || scope.envs.iter().any(|e| {
                account
                    .environment_id
                    .as_ref()
                    .is_some_and(|bound| e != bound)
            })
        {
            return Err(Error::usage(
                "API key is bound to a different organization or environment; select a matching credential",
            ));
        }
        return settings.read_credential(&name);
    }
    let url = base_url(&cli::value(m, "auth-url").unwrap_or_else(|| account.auth_url.clone()))?;
    if url != account.auth_url {
        return Err(Error::usage(
            "Saved credentials cannot be refreshed at a different auth URL",
        ));
    }
    let _lock = settings.lock().await?;
    let mut credential = settings.read_credential(&name)?;
    if credential.expires_at.unwrap_or(0) <= now() + 30 {
        let token = credential
            .refresh_token
            .as_ref()
            .ok_or_else(|| Error::auth("Missing refresh token; log in again"))?;
        let (status, body) = auth_request(client(30)?.post(format!("{url}/oauth/token")).form(&[
            ("client_id", "voltage-cli"),
            ("grant_type", "refresh_token"),
            ("refresh_token", token.as_str()),
        ]))
        .await?;
        require_success(status, &body)?;
        let mut next = token_credential(&body)?;
        next.user_id = credential.user_id.clone();
        next.email = credential.email.clone();
        settings.write_credential(&name, &account.store, &next)?;
        credential = next;
    }
    Ok(credential)
}

pub async fn resolve_organization(
    settings: &Settings,
    scope: &Scope,
    m: &ArgMatches,
) -> Result<Credential> {
    let login = resolve(settings, scope, m).await?;
    if login.api_key.is_some() {
        return Ok(login);
    }
    let organization = scope
        .org
        .as_deref()
        .ok_or_else(|| Error::usage("--org is required"))?;
    let subject = login
        .access_token
        .as_deref()
        .ok_or_else(|| Error::auth("Missing login token; log in again"))?;
    let name = settings.account_name(scope)?;
    let url = base_url(&settings.config.accounts[&name].auth_url)?;
    let (status, body) = auth_request(client(30)?.post(format!("{url}/oauth/token")).form(&[
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:token-exchange",
        ),
        ("subject_token", subject),
        (
            "subject_token_type",
            "urn:ietf:params:oauth:token-type:access_token",
        ),
        ("audience", organization),
    ]))
    .await?;
    require_success(status, &body)?;
    #[derive(Deserialize)]
    struct OrganizationToken {
        access_token: String,
        issued_token_type: String,
        token_type: String,
        expires_in: u64,
    }
    let token: OrganizationToken = serde_json::from_value(body)
        .map_err(|_| Error::auth("Invalid organization token response"))?;
    if token.access_token.is_empty()
        || token.issued_token_type != "urn:ietf:params:oauth:token-type:access_token"
        || !token.token_type.eq_ignore_ascii_case("bearer")
        || token.expires_in == 0
    {
        return Err(Error::auth("Invalid organization token response"));
    }
    // Keep login and refresh credentials in their account store. Organization
    // tokens belong to this command and are never reused for another organization.
    Ok(Credential {
        access_token: Some(token.access_token),
        expires_at: Some(now().saturating_add(token.expires_in)),
        api_key: None,
        refresh_token: None,
        user_id: None,
        email: None,
    })
}

pub async fn logout(settings: &mut Settings, scope: &Scope, m: &ArgMatches) -> Result<Value> {
    let _lock = settings.lock().await?;
    settings.config = Settings::load(m)?.config;
    let name = settings.account_name(scope)?;
    let account = &settings.config.accounts[&name];
    if account.kind == "user" && !cli::enabled(m, "local") {
        let credential = settings.read_credential(&name)?;
        let token = credential.refresh_token.as_ref().ok_or_else(|| {
            Error::auth("Missing refresh token; use --local to remove local credentials")
        })?;
        let (status, body) = auth_request(
            client(30)?
                .post(format!("{}/oauth/revoke", base_url(&account.auth_url)?))
                .form(&[
                    ("client_id", "voltage-cli"),
                    ("token_type_hint", "refresh_token"),
                    ("token", token.as_str()),
                ]),
        )
        .await?;
        require_success(status, &body)?;
    }
    settings.delete_credential(&name)?;
    settings.config.accounts.remove(&name);
    settings.save()?;
    Ok(json!({"account":name,"removed":true,"local_only":cli::enabled(m,"local")}))
}

pub async fn import_key(settings: &mut Settings, scope: &Scope, m: &ArgMatches) -> Result<Value> {
    let name = cli::value(m, "account")
        .ok_or_else(|| Error::usage("--account is required to name the saved API key"))?;
    let org = scope
        .org
        .clone()
        .ok_or_else(|| Error::usage("--org is required"))?;
    if scope.envs.len() != 1 {
        return Err(Error::usage("Exactly one --env is required"));
    }
    let key = if cli::enabled(m, "stdin") {
        crate::config::read_secret("-")?
    } else {
        use std::io::IsTerminal;
        if !std::io::stdin().is_terminal() {
            return Err(Error::usage(
                "Use --stdin to import an API key noninteractively",
            ));
        }
        rpassword::prompt_password("Environment API key: ").map_err(Error::io)?
    };
    if key.trim().is_empty() {
        return Err(Error::usage("API key cannot be empty"));
    }
    let _lock = settings.lock().await?;
    settings.config = Settings::load(m)?.config;
    if settings.config.accounts.contains_key(&name) {
        return Err(Error::usage("Credential already exists"));
    }
    let store = cli::value(m, "credential-store").unwrap_or_else(|| "keychain".into());
    settings.write_credential(
        &name,
        &store,
        &Credential {
            api_key: Some(key),
            access_token: None,
            refresh_token: None,
            expires_at: None,
            user_id: None,
            email: None,
        },
    )?;
    settings.config.accounts.insert(
        name.clone(),
        Account {
            store,
            kind: "api_key".into(),
            email: None,
            organization_id: Some(org),
            environment_id: Some(scope.envs[0].clone()),
            auth_url: AUTH_URL.into(),
        },
    );
    settings.save()?;
    Ok(json!({"account":name,"imported":true}))
}

pub async fn discover(
    settings: &Settings,
    scope: &Scope,
    m: &ArgMatches,
    organizations: bool,
) -> Result<Value> {
    let cred = if organizations {
        resolve(settings, scope, m).await?
    } else {
        resolve_organization(settings, scope, m).await?
    };
    let token = cred
        .access_token
        .as_ref()
        .ok_or_else(|| Error::auth("Organization/environment discovery requires a user login"))?;
    let name = settings.account_name(scope)?;
    let url = base_url(&settings.config.accounts[&name].auth_url)?;
    let path = if organizations {
        "/users/current".into()
    } else {
        format!(
            "/organizations/{}/environments",
            scope
                .org
                .as_ref()
                .ok_or_else(|| Error::usage("--org is required"))?
        )
    };
    let (status, body) =
        auth_request(client(30)?.get(format!("{url}{path}")).bearer_auth(token)).await?;
    require_success(status, &body)?;
    Ok(if organizations {
        body["organizations"].clone()
    } else {
        body
    })
}
