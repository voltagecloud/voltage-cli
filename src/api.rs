use crate::{
    Error, Result, auth, cli,
    config::{self, Credential, Scope, Settings},
    input,
    output::{self, Output},
    registry::Operation,
};
use clap::ArgMatches;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    io::{IsTerminal, Write},
    time::Duration,
};

pub struct Response {
    pub status: u16,
    pub body: Value,
    pub retry_after: Option<Duration>,
}
pub struct Api {
    client: reqwest::Client,
    base: String,
    credential: Credential,
    checkout_token: Option<String>,
    origin: Option<String>,
}
impl Api {
    pub fn new(
        m: &ArgMatches,
        credential: Credential,
        checkout_token: Option<String>,
    ) -> Result<Self> {
        let timeout = *m.get_one::<u64>("timeout").unwrap_or(&60);
        let origin = cli::value(m, "origin");
        if let Some(origin) = &origin {
            let u = url::Url::parse(origin).map_err(|_| Error::usage("Invalid Origin"))?;
            if u.origin().ascii_serialization() != *origin {
                return Err(Error::usage(
                    "Origin must contain only scheme, host, and optional port",
                ));
            }
        }
        Ok(Self {
            client: auth::client(timeout)?,
            base: auth::base_url(
                &cli::value(m, "api-url").unwrap_or_else(|| config::API_URL.into()),
            )?,
            credential,
            checkout_token,
            origin,
        })
    }
    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        auth_scheme: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let mut request = self
            .client
            .request(
                method.parse::<reqwest::Method>().map_err(Error::io)?,
                format!("{}{path}", self.base),
            )
            .query(query);
        match auth_scheme {
            "account" => {
                if let Some(key) = &self.credential.api_key {
                    request = request.header("x-api-key", key);
                } else if let Some(token) = &self.credential.access_token {
                    request = request.bearer_auth(token);
                } else {
                    return Err(Error::auth("No account credential available"));
                }
            }
            "checkout_session" => {
                request =
                    request.bearer_auth(self.checkout_token.as_ref().ok_or_else(|| {
                        Error::auth("Supply --token-file or VOLTAGE_CHECKOUT_TOKEN")
                    })?)
            }
            "checkout_stream" => {
                request = request.query(&[(
                    "stream_token",
                    self.checkout_token.as_ref().ok_or_else(|| {
                        Error::auth("Supply --token-file or VOLTAGE_STREAM_TOKEN")
                    })?,
                )])
            }
            "none" => {}
            _ => return Err(Error::usage("Unknown authentication scheme")),
        }
        if let Some(origin) = &self.origin {
            request = request.header("Origin", origin);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        Ok(request)
    }
    pub async fn once(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
        scheme: &str,
    ) -> Result<Response> {
        let read_only = matches!(method, "GET" | "HEAD" | "OPTIONS");
        let response = self
            .request(method, path, query, body, scheme)?
            .send()
            .await
            .map_err(|_| {
                Error::io(if read_only {
                    "HTTP read request failed. Check the API URL and whether the API service is running."
                } else {
                    "HTTP request failed; a write may have been submitted. No mutation was retried."
                })
            })?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("x-retry-after-ms")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_millis)
            .or_else(|| {
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| {
                        s.parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
                            httpdate::parse_http_date(s).ok().map(|t| {
                                t.duration_since(std::time::SystemTime::now())
                                    .unwrap_or_default()
                            })
                        })
                    })
            });
        let bytes = response.bytes().await.map_err(|_| {
            Error::io(if read_only {
                "HTTP read response was interrupted; retry this read."
            } else {
                "Response was interrupted; reconcile writes by their original ID"
            })
        })?;
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).map_err(|_| {
                Error::new(
                    if (200..300).contains(&status) {
                        4
                    } else if status == 401 || status == 403 {
                        3
                    } else {
                        1
                    },
                    format!("HTTP {status}: non-JSON response omitted"),
                )
            })?
        };
        if !(200..300).contains(&status) {
            let mut detail = json!({"http_status":status,"data":body});
            output::redact(&mut detail, &self.secrets());
            return Err(Error::new(
                if status == 401 || status == 403 { 3 } else { 1 },
                format!("Voltage API returned HTTP {status}"),
            )
            .detail(detail));
        }
        Ok(Response {
            status,
            body,
            retry_after,
        })
    }
    fn secrets(&self) -> Vec<&str> {
        self.credential
            .api_key
            .iter()
            .chain(self.credential.access_token.iter())
            .chain(self.credential.refresh_token.iter())
            .chain(self.checkout_token.iter())
            .map(String::as_str)
            .collect()
    }
    async fn stream(&self, path: &str, query: &[(String, String)], out: &mut Output) -> Result<()> {
        let response = self
            .request("GET", path, query, None, "checkout_stream")?
            .send()
            .await
            .map_err(|_| Error::io("Could not open checkout event stream"))?;
        if !response.status().is_success() {
            return Err(Error::new(
                if matches!(response.status().as_u16(), 401 | 403) {
                    3
                } else {
                    1
                },
                format!("Event stream returned HTTP {}", response.status().as_u16()),
            ));
        }
        if !response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
        {
            return Err(Error::io("Expected text/event-stream"));
        }
        let mut stream = response.bytes_stream();
        let mut pending = Vec::new();
        let mut event = String::new();
        let mut data = Vec::new();
        let mut id = None;
        while let Some(bytes) = stream.next().await {
            pending
                .extend_from_slice(&bytes.map_err(|_| Error::io("Checkout stream disconnected"))?);
            if pending.len() > 1024 * 1024 {
                return Err(Error::io("Event stream line exceeds 1 MiB"));
            }
            while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                let raw: Vec<_> = pending.drain(..=end).collect();
                let line = std::str::from_utf8(&raw)
                    .map_err(|_| Error::io("Invalid UTF-8 in event stream"))?
                    .trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    if !data.is_empty() {
                        let joined = data.join("\n");
                        let value = serde_json::from_str(&joined).unwrap_or(Value::String(joined));
                        let mut envelope = output::envelope(Some(200), value, None, "event");
                        envelope["event"] =
                            json!(if event.is_empty() { "message" } else { &event });
                        envelope["id"] = json!(id);
                        out.write(envelope, &self.secrets())?;
                        data.clear();
                    }
                    event.clear();
                } else if let Some(v) = line.strip_prefix("event:") {
                    event = v.strip_prefix(' ').unwrap_or(v).into();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data.push(v.strip_prefix(' ').unwrap_or(v).to_owned());
                    if data.iter().map(String::len).sum::<usize>() > 1024 * 1024 {
                        return Err(Error::io("Event exceeds 1 MiB"));
                    }
                } else if let Some(v) = line.strip_prefix("id:") {
                    id = Some(v.strip_prefix(' ').unwrap_or(v).to_owned());
                }
            }
        }
        Ok(())
    }
}

pub async fn execute(
    op: &Operation,
    command: &[String],
    m: &ArgMatches,
    settings: &Settings,
    scope: &Scope,
    out: &mut Output,
) -> Result<()> {
    let body = input::body(op, command, m, scope)?;
    let path = input::path(op, m, scope)?;
    let mut query = input::query(op, m, scope)?;
    if [
        "create_webhook",
        "generate_webhook_key",
        "create_session",
        "create_event_stream_token",
    ]
    .contains(&op.id.as_str())
        && !out.secure_destination()
    {
        return Err(Error::usage(
            "This operation returns a one-time secret; supply --output-file PATH or --show-secrets before executing",
        ));
    }
    let credential = if op.auth == "account" {
        auth::resolve(settings, scope, m).await?
    } else {
        Credential::default()
    };
    let checkout_token = if op.auth.starts_with("checkout") {
        if let Some(source) = cli::value(m, "token-file") {
            Some(config::read_secret(&source)?)
        } else {
            let name = if op.auth == "checkout_session" {
                "VOLTAGE_CHECKOUT_TOKEN"
            } else {
                "VOLTAGE_STREAM_TOKEN"
            };
            Some(
                std::env::var(name)
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| Error::auth(format!("Supply --token-file or {name}")))?,
            )
        }
    } else {
        None
    };
    let api = Api::new(m, credential, checkout_token)?;
    let duration = Duration::from_secs(*m.get_one::<u64>("timeout").unwrap_or(&60));
    let deadline = tokio::time::Instant::now() + duration;
    let id = body
        .as_ref()
        .and_then(|b| b["id"].as_str())
        .map(String::from)
        .or_else(|| cli::value(m, "resource-id"));
    if op.target.as_deref() == Some("wallet_id") && op.method != "GET" && !scope.envs.is_empty() {
        let expected = input::single_env(scope)?;
        let wallet_path = format!(
            "/organizations/{}/wallets/{}",
            scope.org.as_ref().unwrap(),
            cli::value(m, "resource-id").unwrap()
        );
        let wallet = api.once("GET", &wallet_path, &[], None, "account").await?;
        if wallet.body["environment_id"].as_str() != Some(expected) {
            return Err(Error::usage(
                "Wallet does not belong to the selected environment",
            ));
        }
    }
    if input::consequential(op, body.as_ref()) {
        confirm(op, scope, body.as_ref(), id.as_deref(), m)?;
    }
    if op.method != "GET"
        && let Some(id) = body.as_ref().and_then(|b| b["id"].as_str())
    {
        journal(settings, op, scope, body.as_ref().unwrap(), id)?;
    }
    if op.auth == "checkout_stream" {
        return api.stream(&path, &query, out).await;
    }
    let result = api
        .once(&op.method, &path, &query, body.as_ref(), &op.auth)
        .await;
    let mut reconciled = false;
    let mut response = match result {
        Ok(r) => Ok(r),
        Err(e) => {
            if e.code == 4 && ["create_payment", "create_treasury_movement"].contains(&op.id.as_str())
                && let (Some(id), Some(org), Ok(env)) = (id.as_ref(), scope.org.as_ref(), input::single_env(scope))
            {
                // A read can establish acceptance without sending the mutation again.
                // A missing projection remains uncertain; retain the original recovery record.
                let payment_path = format!("/organizations/{org}/environments/{env}/payments/{id}");
                if let Ok(Ok(found)) = tokio::time::timeout(Duration::from_secs(5), api.once("GET", &payment_path, &[], None, "account")).await
                    && found.body["id"].as_str() == Some(id)
                {
                    eprintln!("Payment {id} is visible after the interrupted submission; reconciled by reading its original ID.");
                    reconciled = true;
                    Ok(found)
                } else {
                    Err(e)
                }
            } else {
                Err(e)
            }
        }
    }.map_err(|mut e| {
            if op.method != "GET" && e.code == 4 {
                e.detail = Some(
                    json!({"resource_id":id,"organization_id":scope.org,"environment_ids":scope.envs,"outcome":"unknown","action":"Query the original resource ID before deciding whether to resubmit"}),
                );
            }
            e
    })?;
    if op.target.as_deref() == Some("wallet_id")
        && op.id == "get_wallet"
        && !scope.envs.is_empty()
        && response.body["environment_id"].as_str() != Some(input::single_env(scope)?)
    {
        return Err(Error::usage(
            "Wallet does not belong to the selected environment",
        ));
    }
    if let Some(until) = cli::value(m, "wait") {
        let id = id
            .as_ref()
            .ok_or_else(|| Error::usage("Waiting requires a payment ID"))?;
        let payment_path = format!(
            "/organizations/{}/environments/{}/payments/{id}",
            scope
                .org
                .as_ref()
                .ok_or_else(|| Error::usage("--org is required"))?,
            input::single_env(scope)?
        );
        if op.method != "GET" {
            eprintln!("Payment {id} accepted; waiting for {until}.");
        }
        let mut pause = Duration::from_secs(1);
        loop {
            let state = wait_state(&response.body, &until).map_err(|mut error| {
                if let Some(detail) = &mut error.detail {
                    output::redact(detail, &api.secrets());
                }
                error
            })?;
            if let Some(outcome) = state {
                return out.write(
                    output::envelope(
                        Some(response.status),
                        response.body,
                        Some(id.clone()),
                        outcome,
                    ),
                    &api.secrets(),
                );
            }
            if tokio::time::Instant::now() + pause >= deadline {
                return Err(Error::new(
                    5,
                    format!("Payment {id} is still pending; waiting timed out"),
                )
                .detail(json!({"resource_id":id,"outcome":"pending"})));
            }
            tokio::time::sleep(pause).await;
            match tokio::time::timeout_at(
                deadline,
                api.once("GET", &payment_path, &[], None, "account"),
            )
            .await
            {
                Ok(Ok(r)) => {
                    response = r;
                    pause = response.retry_after.unwrap_or(Duration::from_secs(1));
                }
                Ok(Err(e)) if e.detail.as_ref().is_some_and(|v| v["http_status"] == 404) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(Error::new(5, format!("Payment {id} wait timed out"))
                        .detail(json!({"resource_id":id,"outcome":"pending"})));
                }
            }
        }
    }
    if op.id == "get_session" {
        while response.status == 202 {
            let pause = response.retry_after.unwrap_or(Duration::from_millis(500));
            if tokio::time::Instant::now() + pause >= deadline {
                return Err(Error::new(5, "Checkout session projection is not ready"));
            }
            tokio::time::sleep(pause).await;
            response =
                tokio::time::timeout_at(deadline, api.once("GET", &path, &query, None, &op.auth))
                    .await
                    .map_err(|_| Error::new(5, "Checkout session wait timed out"))??;
        }
    }
    let all = cli::enabled(m, "all");
    let mut pages = Vec::new();
    let mut cursors = std::collections::BTreeSet::new();
    loop {
        let next = if all {
            next_page(&response.body, &query)?
        } else {
            None
        };
        let outcome = if response.status == 202 || reconciled {
            "accepted"
        } else if op.method == "GET" {
            "retrieved"
        } else {
            "succeeded"
        };
        let envelope = output::envelope(Some(response.status), response.body, id.clone(), outcome);
        if all && out.format == "json" {
            pages.push(envelope);
        } else {
            out.write(envelope, &api.secrets())?;
        }
        let Some((name, value)) = next else {
            break;
        };
        if !cursors.insert((name.clone(), value.clone())) {
            return Err(Error::io("Pagination did not advance"));
        }
        query.retain(|(k, _)| k != &name);
        query.push((name, value));
        response =
            tokio::time::timeout_at(deadline, api.once("GET", &path, &query, None, &op.auth))
                .await
                .map_err(|_| {
                    Error::new(5, "Pagination deadline exceeded; use a longer --timeout")
                })??;
    }
    if all && out.format == "json" {
        out.write(
            output::envelope(Some(200), json!({"pages":pages}), None, "retrieved"),
            &api.secrets(),
        )?;
    }
    Ok(())
}

pub fn wait_state(body: &Value, until: &str) -> Result<Option<&'static str>> {
    match body["status"].as_str() {
        Some("failed" | "expired") => Err(Error::new(
            1,
            "Payment reached an unsuccessful terminal state",
        )
        .detail(body.clone())),
        Some("completed") => Ok(Some("completed")),
        Some("receiving") if until == "ready" => Ok(Some("ready")),
        _ => Ok(None),
    }
}
fn next_page(body: &Value, query: &[(String, String)]) -> Result<Option<(String, String)>> {
    let offset_mode = query
        .iter()
        .any(|(k, v)| k == "offset" || k == "pagination" && v == "offset");
    if !offset_mode && (body.get("has_more").is_some() || body.get("next_cursor").is_some()) {
        if body["has_more"] == false {
            return Ok(None);
        }
        if let Some(cursor) = body["next_cursor"].as_str().filter(|s| !s.is_empty()) {
            return Ok(Some(("cursor".into(), cursor.into())));
        }
        if body["has_more"] == true {
            return Err(Error::io("API indicates more pages but returned no cursor"));
        }
        return Ok(None);
    }
    if body["has_more"] == false {
        return Ok(None);
    }
    let Some(items) = body["items"]
        .as_array()
        .or_else(|| body["entries"].as_array())
    else {
        return Ok(None);
    };
    if items.is_empty() {
        return Ok(None);
    }
    let offset = body["offset"]
        .as_u64()
        .or_else(|| {
            query
                .iter()
                .find(|(k, _)| k == "offset")
                .and_then(|(_, v)| v.parse().ok())
        })
        .unwrap_or(0);
    let next = offset
        .checked_add(items.len() as u64)
        .ok_or_else(|| Error::io("Pagination overflow"))?;
    if body["total"].as_u64().is_some_and(|t| next >= t)
        || body["limit"]
            .as_u64()
            .is_some_and(|l| (items.len() as u64) < l)
    {
        return Ok(None);
    }
    Ok(Some(("offset".into(), next.to_string())))
}
fn confirm(
    op: &Operation,
    scope: &Scope,
    body: Option<&Value>,
    id: Option<&str>,
    m: &ArgMatches,
) -> Result<()> {
    let mut summary = json!({"action":op.command.join(" "),"organization":scope.org,"environments":scope.envs,"resource_id":id,"request":body});
    output::redact(&mut summary, &[]);
    eprintln!("{}", serde_json::to_string_pretty(&summary)?);
    if op.id == "create_payment" {
        eprintln!(
            "Network/provider fee limits exclude additional processing fees. The wallet determines the network."
        );
    }
    if cli::enabled(m, "yes") {
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
        return Err(Error::new(2, "Operation cancelled before submission"));
    }
    Ok(())
}
fn journal(
    settings: &Settings,
    op: &Operation,
    scope: &Scope,
    body: &Value,
    id: &str,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    config::private_dir(&settings.dir)?;
    let dir = settings.dir.join("requests");
    config::private_dir(&dir)?;
    let hash = hex::encode(Sha256::digest(serde_json::to_vec(body)?));
    let path = dir.join(format!("{id}.json"));
    let record = json!({"resource_id":id,"operation":op.id,"organization_id":scope.org,"environment_ids":scope.envs,"request_sha256":hash,"created_at":auth::now()});
    if path.exists() {
        let old: Value = serde_json::from_str(&config::read_private(&path)?)?;
        if old["request_sha256"] != record["request_sha256"]
            || old["organization_id"] != record["organization_id"]
            || old["environment_ids"] != record["environment_ids"]
            || old["operation"] != record["operation"]
        {
            return Err(Error::usage(
                "This resource ID was previously used for a different request; reconcile it before proceeding",
            ));
        }
        return Ok(());
    }
    let mut file = config::new_private(&path)?;
    file.write_all(&serde_json::to_vec(&record)?)?;
    file.sync_all()?;
    eprintln!("Resource ID: {id}. Recovery record: {}", path.display());
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readiness_is_not_completion() {
        let b = json!({"status":"receiving"});
        assert_eq!(wait_state(&b, "ready").unwrap(), Some("ready"));
        assert_eq!(wait_state(&b, "completed").unwrap(), None);
        assert!(wait_state(&json!({"status":"failed"}), "completed").is_err());
    }
    #[test]
    fn cursor_pagination_stops() {
        assert!(
            next_page(&json!({"has_more":false}), &[])
                .unwrap()
                .is_none()
        );
        assert!(next_page(&json!({"has_more":true}), &[]).is_err());
    }
}
