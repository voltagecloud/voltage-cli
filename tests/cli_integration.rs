//! Process-level behavior of the `voltage` binary against mock HTTP services.
//!
//! Every test runs the built binary in a private temporary configuration directory with a
//! fixed API key, talks to a wiremock server started on port 0, and never reaches a real
//! service. Payment waits poll with jittered backoff, so tests count requests only where the
//! mocked response sequence makes the count deterministic.

use assert_cmd::{Command, assert::Assert};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
use voltage_cli::{
    config::{
        self, Account, AccountKind, Config, Credential, CredentialStore, Login, Profile, Settings,
    },
    registry::{AuthScheme, Method, OPERATIONS, OperationId, ResourceTarget},
    secret::Secret,
};
use wiremock::{
    Mock, MockBuilder, MockServer, Request, ResponseTemplate,
    matchers::{
        body_json, body_string_contains, header, method, path, query_param, query_param_is_missing,
    },
};

const ORG: &str = "11111111-1111-4111-8111-111111111111";
const ENV: &str = "22222222-2222-4222-8222-222222222222";
const WALLET: &str = "33333333-3333-4333-8333-333333333333";
const RESOURCE: &str = "44444444-4444-4444-8444-444444444444";
/// The ambient API key every command starts with unless a test removes it.
const ACCOUNT_KEY: &str = "test-account-key";
const CHECKOUT_KEY: &str = "checkout-key";
const STREAM_KEY: &str = "stream-key";
/// The saved login seeded by `seeded_login`.
const LOGIN_NAME: &str = "person";
const LOGIN_EMAIL: &str = "person@example.test";
const LOGIN_ACCESS: &str = "old-access";
const LOGIN_REFRESH: &str = "old-refresh";
const TOKEN_EXCHANGE_GRANT: &str = "urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aaccess_token";

// ---------------------------------------------------------------------------------------
// Help, parse errors, and pipelines
// ---------------------------------------------------------------------------------------

#[test]
fn typo_errors_are_human_by_default_and_json_when_requested() {
    let human = Command::new(assert_cmd::cargo::cargo_bin!("voltage"))
        .arg("paymnts")
        .assert()
        .code(2);
    assert!(human.get_output().stdout.is_empty());
    let human_error = stderr(&human);
    assert!(human_error.starts_with("error:"));
    assert!(human_error.contains("similar subcommand exists: 'payments'"));

    let machine = Command::new(assert_cmd::cargo::cargo_bin!("voltage"))
        .args(["--json", "paymnts"])
        .assert()
        .code(2);
    assert!(machine.get_output().stdout.is_empty());
    let report: Value = serde_json::from_slice(&machine.get_output().stderr).unwrap();
    assert_eq!(report["error"]["exit_code"], 2);
    assert!(
        report["error"]["message"]
            .as_str()
            .unwrap()
            .contains("similar subcommand exists: 'payments'")
    );
}

#[test]
fn help_and_version_stay_on_stdout_when_json_is_present() {
    for flag in ["--help", "--version"] {
        let result = Command::new(assert_cmd::cargo::cargo_bin!("voltage"))
            .args(["--json", flag])
            .assert()
            .success();
        assert!(!result.get_output().stdout.is_empty());
        assert!(result.get_output().stderr.is_empty());
    }
}

#[test]
fn version_leads_with_the_cargo_version_and_marks_the_build_separately() {
    let result = Command::new(assert_cmd::cargo::cargo_bin!("voltage"))
        .arg("--version")
        .assert()
        .success();
    let version = stdout(&result);
    let line = version.trim_end();
    let prefix = format!("voltage {}", env!("CARGO_PKG_VERSION"));
    // Release tooling and the Homebrew test read the first token; git detail may follow.
    let build = line.strip_prefix(&prefix).unwrap();
    assert!(
        build.is_empty() || (build.starts_with(" (") && build.ends_with(')')),
        "{line}"
    );
}

#[test]
fn human_runtime_errors_have_a_prefix_and_safe_hint() {
    let dir = private_tempdir();
    let result = Command::new(assert_cmd::cargo::cargo_bin!("voltage"))
        .arg("--config-dir")
        .arg(dir.path())
        .args(["--output", "table", "wallets", "list"])
        .assert()
        .code(2);
    let error = stderr(&result);
    assert!(
        error.starts_with("error: --org is required for wallets list\n"),
        "{error}"
    );
    assert!(error.contains("hint: Pass --org UUID or select a profile"));
    assert!(!error.contains("api_key"));
}

#[cfg(unix)]
#[test]
fn completion_output_treats_an_early_closing_pipe_as_success() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command as Process, Stdio};

    let mut child = Process::new(assert_cmd::cargo::cargo_bin!("voltage"))
        .args(["completions", "bash"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut first_line = String::new();
    stdout.read_line(&mut first_line).unwrap();
    assert!(!first_line.is_empty());
    drop(stdout);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
}

// ---------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------

/// A mock server and an owner-only configuration directory for one test.
async fn fixture() -> (MockServer, TempDir) {
    (MockServer::start().await, private_tempdir())
}

fn private_tempdir() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

/// The binary with the ambient API key, JSON output, and the given API server.
fn cli(dir: &Path, server: &MockServer) -> Command {
    cli_at(dir, &server.uri())
}

fn cli_at(dir: &Path, api_url: &str) -> Command {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("voltage"));
    for name in [
        "VOLTAGE_ORGANIZATION_ID",
        "VOLTAGE_ENVIRONMENT_ID",
        "VOLTAGE_WALLET_ID",
        "VOLTAGE_API_KEY",
        "VOLTAGE_CHECKOUT_TOKEN",
        "VOLTAGE_STREAM_TOKEN",
    ] {
        cmd.env_remove(name);
    }
    cmd.env("VOLTAGE_API_KEY", ACCOUNT_KEY)
        .arg("--config-dir")
        .arg(dir)
        .arg("--api-url")
        .arg(api_url)
        .arg("--json");
    cmd
}

fn json_stdout(assertion: &Assert) -> Value {
    serde_json::from_slice(&assertion.get_output().stdout).unwrap()
}

fn stdout(assertion: &Assert) -> String {
    String::from_utf8_lossy(&assertion.get_output().stdout).into_owned()
}

fn stderr(assertion: &Assert) -> String {
    String::from_utf8_lossy(&assertion.get_output().stderr).into_owned()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn uuid(text: &str) -> uuid::Uuid {
    text.parse().unwrap()
}

fn payments_path() -> String {
    format!("/organizations/{ORG}/environments/{ENV}/payments")
}

fn payment_path() -> String {
    format!("{}/{RESOURCE}", payments_path())
}

fn wallets_path(org: &str) -> String {
    format!("/organizations/{org}/wallets")
}

/// A request matcher for one method and path.
fn route(verb: &str, route: impl Into<String>) -> MockBuilder {
    Mock::given(method(verb)).and(path(route.into()))
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn accepted() -> ResponseTemplate {
    ResponseTemplate::new(202)
}

/// Mount a mock that must be hit exactly `times` times.
async fn respond(
    server: &MockServer,
    request: MockBuilder,
    response: ResponseTemplate,
    times: u64,
) {
    request
        .respond_with(response)
        .expect(times)
        .mount(server)
        .await;
}

async fn respond_once(server: &MockServer, request: MockBuilder, response: ResponseTemplate) {
    respond(server, request, response, 1).await;
}

/// Answer successive requests with successive templates; the last one repeats.
fn in_order(responses: Vec<ResponseTemplate>) -> impl Fn(&Request) -> ResponseTemplate {
    let count = Arc::new(AtomicUsize::new(0));
    move |_: &Request| {
        let index = count
            .fetch_add(1, Ordering::SeqCst)
            .min(responses.len() - 1);
        responses[index].clone()
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn result_output_treats_an_early_closing_pipe_as_success_without_resubmitting() {
    use std::process::{Command as Process, Stdio};

    let (server, dir) = fixture().await;
    respond_once(
        &server,
        route("POST", payments_path()),
        ok(json!({
            "id": RESOURCE,
            "status": "receiving",
            "padding": "x".repeat(1024 * 1024)
        })),
    )
    .await;
    let mut command = Process::new(assert_cmd::cargo::cargo_bin!("voltage"));
    for name in [
        "VOLTAGE_ORGANIZATION_ID",
        "VOLTAGE_ENVIRONMENT_ID",
        "VOLTAGE_WALLET_ID",
        "VOLTAGE_CHECKOUT_TOKEN",
        "VOLTAGE_STREAM_TOKEN",
    ] {
        command.env_remove(name);
    }
    let mut child = command
        .env("VOLTAGE_API_KEY", ACCOUNT_KEY)
        .arg("--config-dir")
        .arg(dir.path())
        .args([
            "--api-url",
            &server.uri(),
            "--json",
            "--org",
            ORG,
            "--env",
            ENV,
            "--wallet",
            WALLET,
            "payments",
            "receive",
            "--currency",
            "btc",
            "--kind",
            "bolt11",
            "--amount",
            "1",
            "--unit",
            "sats",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Broken pipe"));
    server.verify().await;
}

fn settings_at(dir: &Path) -> Settings {
    Settings {
        dir: dir.into(),
        config: Config::default(),
    }
}

/// A saved browser login stored in a private file, expired or good for ten minutes.
fn seeded_login(dir: &Path, auth: &MockServer, expired: bool) -> Settings {
    let mut settings = settings_at(dir);
    settings.config.accounts.insert(
        LOGIN_NAME.into(),
        Account {
            store: CredentialStore::File,
            kind: AccountKind::User,
            email: Some(LOGIN_EMAIL.into()),
            organization_id: None,
            environment_id: None,
            auth_url: auth.uri(),
        },
    );
    settings
        .write_credential(
            LOGIN_NAME,
            CredentialStore::File,
            &Credential::Login(Login {
                access_token: Secret::new(LOGIN_ACCESS.into()),
                refresh_token: Secret::new(LOGIN_REFRESH.into()),
                expires_at: if expired { 0 } else { unix_now() + 600 },
                user_id: Some(RESOURCE.into()),
                email: Some(LOGIN_EMAIL.into()),
            }),
        )
        .unwrap();
    settings.save().unwrap();
    settings
}

/// A saved API key stored in a private file and bound to `ORG` and `ENV`.
fn seeded_api_key(dir: &Path, name: &str, key: &str) -> Settings {
    let mut settings = settings_at(dir);
    settings.config.accounts.insert(
        name.into(),
        Account {
            store: CredentialStore::File,
            kind: AccountKind::ApiKey,
            email: None,
            organization_id: Some(uuid(ORG)),
            environment_id: Some(uuid(ENV)),
            auth_url: config::AUTH_URL.into(),
        },
    );
    settings
        .write_credential(
            name,
            CredentialStore::File,
            &Credential::ApiKey(Secret::new(key.into())),
        )
        .unwrap();
    settings.save().unwrap();
    settings
}

/// The saved login's access and refresh tokens.
fn login_tokens(settings: &Settings, name: &str) -> (String, String) {
    match settings.read_credential(name).unwrap() {
        Credential::Login(login) => (
            login.access_token.expose().to_owned(),
            login.refresh_token.expose().to_owned(),
        ),
        Credential::ApiKey(_) => panic!("expected a login credential"),
    }
}

/// Mount the organization token exchange for `login` and `org`, answering `count` times.
async fn organization_exchange(
    auth: &MockServer,
    login: &str,
    org: &str,
    access: &str,
    count: u64,
) {
    route("POST", "/oauth/token")
        .and(body_string_contains(format!(
            "grant_type={TOKEN_EXCHANGE_GRANT}"
        )))
        .and(body_string_contains(format!(
            "subject_token_type={ACCESS_TOKEN_TYPE}"
        )))
        .and(body_string_contains(format!("subject_token={login}")))
        .and(body_string_contains(format!("audience={org}")))
        .respond_with(ok(json!({
            "access_token": access,
            "issued_token_type": "urn:ietf:params:oauth:token-type:access_token",
            "token_type": "Bearer", "expires_in": 600, "scope": "read write"
        })))
        .expect(count)
        .up_to_n_times(count)
        .mount(auth)
        .await;
}

/// Arguments for a friendly BOLT11 receive of `RESOURCE` in `ORG`/`ENV`.
fn receive_args<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "payments",
        "receive",
        "--org",
        ORG,
        "--env",
        ENV,
        "--id",
        RESOURCE,
        "--currency",
        "btc",
        "--kind",
        "bolt11",
    ];
    args.extend_from_slice(extra);
    args
}

// ---------------------------------------------------------------------------------------
// Contract coverage
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_documented_operation_reaches_its_exact_route_and_auth_scheme() {
    for op in OPERATIONS.iter() {
        let (server, dir) = fixture().await;
        let target = if op.target == Some(ResourceTarget::WalletId) {
            WALLET
        } else {
            RESOURCE
        };
        let route_path = op
            .path
            .replace("{organization_id}", ORG)
            .replace("{environment_id}", ENV)
            .replace("{wallet_id}", WALLET)
            .replace("{webhook_id}", RESOURCE)
            .replace("{payment_id}", RESOURCE)
            .replace("{quote_id}", RESOURCE)
            .replace("{line_id}", RESOURCE)
            .replace("{bill_id}", RESOURCE)
            .replace("{delivery_id}", RESOURCE)
            .replace("{session_id}", RESOURCE);
        let body = if op.id == OperationId::CreateWallet {
            json!({"id":RESOURCE,"environment_id":ENV,"line_of_credit_id":RESOURCE,"name":"wallet","network":"mutinynet","limit":0})
        } else {
            json!({"id":RESOURCE,"wallet_id":WALLET,"currency":"btc","type":"bolt11","data":{"payment_request":"example","amount":{"currency":"btc","amount":9223372036854775807i64}},"extension":{"preserved":true}})
        };
        let response = if op.id == OperationId::GetEvents {
            ResponseTemplate::new(200).set_body_raw(
                "event: updated\ndata: {\"status\":\"completed\"}\n\n",
                "text/event-stream",
            )
        } else if op.method == Method::Get {
            ok(json!({"id":target,"environment_id":ENV,"extra":"preserved"}))
        } else {
            accepted()
        };
        let mut mock = route(op.method.as_str(), route_path.clone());
        if op.auth == AuthScheme::Account {
            mock = mock.and(header("x-api-key", ACCOUNT_KEY));
        }
        if op.auth == AuthScheme::CheckoutSession {
            mock = mock.and(header("authorization", format!("Bearer {CHECKOUT_KEY}")));
        }
        if op.body {
            mock = mock.and(body_json(body.clone()));
        }
        respond_once(&server, mock, response).await;
        if op.target == Some(ResourceTarget::WalletId) && op.method != Method::Get {
            // Wallet mutations verify the wallet's environment with a read first.
            respond_once(
                &server,
                route("GET", route_path.split("/policies").next().unwrap()),
                ok(json!({"environment_id":ENV})),
            )
            .await;
        }
        let mut cmd = cli(dir.path(), &server);
        cmd.args(&op.command)
            .args(["--org", ORG, "--env", ENV, "--yes", "--show-secrets"]);
        if op.target.is_some() {
            cmd.arg(target);
        }
        if op.path.contains("{webhook_id}") && op.target != Some(ResourceTarget::WebhookId) {
            cmd.args(["--webhook", RESOURCE]);
        }
        if op.body {
            let request = dir.path().join("request.json");
            std::fs::write(&request, body.to_string()).unwrap();
            cmd.args(["--data", &format!("@{}", request.display())]);
        }
        if op.auth == AuthScheme::CheckoutSession {
            cmd.env("VOLTAGE_CHECKOUT_TOKEN", CHECKOUT_KEY);
        }
        if op.auth == AuthScheme::CheckoutStream {
            cmd.env("VOLTAGE_STREAM_TOKEN", STREAM_KEY);
        }
        let mut expected_query = Vec::new();
        for p in op.parameters.iter().filter(|p| p.is_query_filter()) {
            // Closed parameters take the contract's first value; the rest are shaped by name.
            let value = match (p.values.first(), p.name.as_str()) {
                (Some(first), _) => first.as_str(),
                (None, "environment_id" | "environment_ids") => ENV,
                (None, "wallet_id") => WALLET,
                (None, "metadata") => "order=a&b=3",
                (None, _) if p.is_boolean() => "true",
                (None, "limit") => "10",
                (None, "offset") => "0",
                (None, "start_date" | "end_date") => "2026-09-09T10:30:00Z",
                (None, _) => RESOURCE,
            };
            if p.name == "wallet_id" {
                cmd.args(["--wallet", value]);
            } else if !p.name.starts_with("environment_id") {
                cmd.args([&format!("--{}", p.flag()), value]);
            }
            let pair = if p.name == "metadata" {
                ("metadata[order]".to_owned(), "a&b=3".to_owned())
            } else {
                let name = if p.is_array() {
                    format!("{}[]", p.name)
                } else {
                    p.name.clone()
                };
                (name, value.to_owned())
            };
            expected_query.push(pair);
        }
        let result = cmd.assert().success();
        if op.method != Method::Get {
            assert_eq!(json_stdout(&result)["outcome"], "accepted", "{:?}", op.id);
        }
        let requests = server.received_requests().await.unwrap();
        let request = requests
            .iter()
            .find(|r| r.method.as_str() == op.method.as_str() && r.url.path() == route_path)
            .unwrap();
        for (name, value) in expected_query {
            assert!(
                request
                    .url
                    .query_pairs()
                    .any(|(k, v)| k == name && v == value),
                "{:?} omitted query parameter {}",
                op.id,
                name
            );
        }
        if op.auth != AuthScheme::Account {
            assert!(
                request.headers.get("x-api-key").is_none(),
                "{:?} leaked account credential",
                op.id
            );
        }
        if matches!(op.auth, AuthScheme::None | AuthScheme::CheckoutStream) {
            assert!(request.headers.get("authorization").is_none());
        }
        if op.auth == AuthScheme::CheckoutStream {
            assert_eq!(
                request
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "stream_token")
                    .unwrap()
                    .1,
                STREAM_KEY
            );
        }
        server.verify().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn query_encoding_preserves_arrays_and_metadata() {
    let (server, dir) = fixture().await;
    respond_once(
        &server,
        Mock::given(method("GET")),
        ok(json!({"items":[],"has_more":false})),
    )
    .await;
    cli(dir.path(), &server)
        .args([
            "payments",
            "list",
            "--org",
            ORG,
            "--env",
            ENV,
            "--statuses",
            "completed",
            "--statuses",
            "failed",
            "--metadata",
            "order=a&b=3",
        ])
        .assert()
        .success();
    let requests = server.received_requests().await.unwrap();
    let query: Vec<_> = requests[0].url.query_pairs().collect();
    assert!(
        query
            .iter()
            .any(|(k, v)| k == "metadata[order]" && v == "a&b=3")
    );
    assert_eq!(
        query
            .iter()
            .filter(|(k, _)| k.starts_with("statuses"))
            .map(|(k, v)| (k.as_ref(), v.as_ref()))
            .collect::<Vec<_>>(),
        [("statuses[]", "completed"), ("statuses[]", "failed")]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_builders_send_documented_event_variants() {
    for action in ["create", "update"] {
        let (server, dir) = fixture().await;
        let mut payload = json!({"events":[
            {"send":"succeeded"}, {"send":"failed"},
            {"receive":"completed"}, {"receive":"failed"}, {"test":"created"}
        ]});
        let mut command = cli(dir.path(), &server);
        command.args(["webhooks", action, "--org", ORG, "--env", ENV]);
        if action == "create" {
            command.args([
                "--id",
                RESOURCE,
                "--name",
                "demo",
                "--url",
                "https://example.test/hook",
                "--show-secrets",
            ]);
            payload["id"] = json!(RESOURCE);
            payload["name"] = json!("demo");
            payload["url"] = json!("https://example.test/hook");
        } else {
            command.arg(RESOURCE);
        }
        for event in [
            "send.succeeded",
            "send.failed",
            "receive.completed",
            "receive.failed",
            "test.created",
        ] {
            command.args(["--event", event]);
        }
        let verb = if action == "create" { "POST" } else { "PATCH" };
        respond_once(
            &server,
            Mock::given(method(verb)).and(body_json(payload)),
            accepted(),
        )
        .await;
        command.assert().success();
        server.verify().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn wallet_network_flags_match_the_wallet_contract() {
    let spec: Value = serde_json::from_str(include_str!("../api/openapi.json")).unwrap();
    for network in spec["components"]["schemas"]["SupportedNetwork"]["enum"]
        .as_array()
        .unwrap()
    {
        let (server, dir) = fixture().await;
        respond_once(
            &server,
            Mock::given(method("POST")).and(body_json(json!({
                "id":RESOURCE,"environment_id":ENV,"line_of_credit_id":RESOURCE,"name":"demo",
                "network":network,"limit":0,"metadata":{}
            }))),
            accepted(),
        )
        .await;
        cli(dir.path(), &server)
            .args([
                "wallets",
                "create",
                "--org",
                ORG,
                "--env",
                ENV,
                "--id",
                RESOURCE,
                "--name",
                "demo",
                "--credit-line",
                RESOURCE,
                "--limit",
                "0",
                "--network",
                network.as_str().unwrap(),
            ])
            .assert()
            .success();
        server.verify().await;
    }
    let (server, dir) = fixture().await;
    for network in ["bitcoin", "testnet", "signet", "regtest"] {
        cli(dir.path(), &server)
            .args(["wallets", "create", "--network", network])
            .assert()
            .code(2);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------
// Scope, profiles, and credential selection
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn profile_credentials_override_ambient_key_and_scope() {
    let (server, dir) = fixture().await;
    let mut settings = seeded_api_key(dir.path(), "stage", "stage-key");
    settings.config.profiles.insert(
        "stage".into(),
        Profile {
            organization_id: uuid(ORG),
            environment_id: uuid(ENV),
            account: "stage".into(),
        },
    );
    settings.save().unwrap();
    respond_once(
        &server,
        route("GET", wallets_path(ORG)).and(header("x-api-key", "stage-key")),
        ok(json!([])),
    )
    .await;
    cli(dir.path(), &server)
        .env("VOLTAGE_ORGANIZATION_ID", RESOURCE)
        .env("VOLTAGE_ENVIRONMENT_ID", RESOURCE)
        .args(["wallets", "list", "--profile", "stage"])
        .assert()
        .success();
    cli(dir.path(), &server)
        .args(["wallets", "list", "--profile", "stage", "--env", RESOURCE])
        .assert()
        .code(2);
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn profiles_are_created_from_scope_and_listed_without_ambient_state() {
    let (server, dir) = fixture().await;
    let settings = seeded_login(dir.path(), &server, false);
    let create = [
        "profiles",
        "create",
        "stage",
        "--org",
        ORG,
        "--env",
        ENV,
        "--account",
        LOGIN_NAME,
    ];
    let created = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(create)
        .assert()
        .success();
    assert_eq!(
        json_stdout(&created)["data"],
        json!({"profile":"stage","created":true})
    );
    cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(create)
        .assert()
        .code(2);
    let listed = cli(dir.path(), &server)
        .args(["profiles", "list"])
        .assert()
        .success();
    assert_eq!(
        json_stdout(&listed)["data"],
        json!({"stage":{"organization_id":ORG,"environment_id":ENV,"account":LOGIN_NAME}})
    );
    let fetched = cli(dir.path(), &server)
        .args(["profiles", "get", "stage"])
        .assert()
        .success();
    assert_eq!(json_stdout(&fetched)["data"]["account"], LOGIN_NAME);
    cli(dir.path(), &server)
        .args(["profiles", "get", "missing"])
        .assert()
        .code(2);
    let deleted = cli(dir.path(), &server)
        .args(["profiles", "delete", "stage"])
        .assert()
        .success();
    assert_eq!(
        json_stdout(&deleted)["data"],
        json!({"profile":"stage","deleted":true})
    );
    let reloaded = Settings::open(settings.dir.clone()).unwrap();
    assert!(reloaded.config.profiles.is_empty());
    assert!(reloaded.config.accounts.contains_key(LOGIN_NAME));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ambiguous_account_selection_and_missing_ids_fail_before_http() {
    let (server, dir) = fixture().await;
    let mut settings = seeded_login(dir.path(), &server, false);
    settings.config.accounts.insert(
        "another".into(),
        settings.config.accounts[LOGIN_NAME].clone(),
    );
    settings.save().unwrap();
    cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .code(2);
    cli(dir.path(), &server)
        .args(["wallets", "get", "--org", ORG])
        .assert()
        .code(2);
    cli(dir.path(), &server)
        .args([
            "payments",
            "receive",
            "--org",
            ORG,
            "--env",
            ENV,
            "--currency",
            "btc",
            "--kind",
            "bolt11",
        ])
        .assert()
        .code(2);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn imported_keys_bind_their_scope_and_report_status_without_secrets() {
    let (server, dir) = fixture().await;
    let import = [
        "auth",
        "import-key",
        "--stdin",
        "--account",
        "staging-key",
        "--org",
        ORG,
        "--env",
        ENV,
    ];
    let imported = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(import)
        .args(["--credential-store", "file"])
        .write_stdin("  imported-secret \n")
        .assert()
        .success();
    assert_eq!(
        json_stdout(&imported)["data"],
        json!({"account":"staging-key","imported":true})
    );
    let settings = Settings::open(dir.path().to_path_buf()).unwrap();
    let account = &settings.config.accounts["staging-key"];
    assert_eq!(account.kind, AccountKind::ApiKey);
    assert_eq!(account.store, CredentialStore::File);
    assert_eq!(account.organization_id, Some(uuid(ORG)));
    let Credential::ApiKey(key) = settings.read_credential("staging-key").unwrap() else {
        panic!("expected an API key");
    };
    assert_eq!(key.expose(), "imported-secret");
    let status = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(["auth", "status"])
        .assert()
        .success();
    let data = &json_stdout(&status)["data"];
    assert_eq!(data["account"], "staging-key");
    assert_eq!(data["kind"], "api_key");
    assert_eq!(data["expired"], false);
    assert!(!stdout(&status).contains("imported-secret"));
    cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(import)
        .write_stdin("again")
        .assert()
        .code(2);
    cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args([
            "auth",
            "import-key",
            "--stdin",
            "--account",
            "empty",
            "--org",
            ORG,
            "--env",
            ENV,
        ])
        .write_stdin("   ")
        .assert()
        .code(2);
    let ambient = cli(dir.path(), &server)
        .args(["auth", "status"])
        .assert()
        .success();
    assert_eq!(
        json_stdout(&ambient)["data"],
        json!({"source":"VOLTAGE_API_KEY","kind":"api_key","configured":true})
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn insecure_file_credentials_fail_without_falling_back() {
    use std::os::unix::fs::PermissionsExt;
    let (server, dir) = fixture().await;
    seeded_login(dir.path(), &server, false);
    let secret = std::fs::read_dir(dir.path().join("credentials"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
    cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG, "--account", LOGIN_NAME])
        .assert()
        .code(2);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_credential_store_failure_never_creates_plaintext_credentials() {
    let (server, dir) = fixture().await;
    let mut settings = settings_at(dir.path());
    settings.config.accounts.insert(
        "missing".into(),
        Account {
            store: CredentialStore::Keychain,
            kind: AccountKind::User,
            email: None,
            organization_id: None,
            environment_id: None,
            auth_url: server.uri(),
        },
    );
    settings.save().unwrap();
    cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG, "--account", "missing"])
        .assert()
        .code(3);
    assert!(!dir.path().join("credentials").exists());
    assert!(server.received_requests().await.unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------
// Browser login, refresh, exchange, and logout
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_device_login_handles_pending_and_slowdown_without_exposing_tokens() {
    let (server, dir) = fixture().await;
    respond_once(
        &server,
        route("POST", "/oauth/device_authorization"),
        ok(json!({
            "device_code":"device-secret","user_code":"ABCD-EFGH",
            "verification_uri":"https://app.voltage.cloud/cli/authorize","expires_in":60,"interval":5
        })),
    )
    .await;
    route("POST", "/oauth/token")
        .respond_with(in_order(vec![
            ResponseTemplate::new(400).set_body_json(json!({"error":"authorization_pending"})),
            ResponseTemplate::new(429).set_body_string("Too Many Requests"),
            ok(json!({
                "access_token":"login-access-secret","refresh_token":"login-refresh-secret",
                "token_type":"Bearer","expires_in":600
            })),
        ]))
        .expect(3)
        .mount(&server)
        .await;
    respond_once(
        &server,
        route("GET", "/users/current").and(header("authorization", "Bearer login-access-secret")),
        ok(json!({"id":RESOURCE,"email":LOGIN_EMAIL})),
    )
    .await;
    let begin = std::time::Instant::now();
    let login = cli(dir.path(), &server)
        .args([
            "login",
            "--auth-url",
            &server.uri(),
            "--no-browser",
            "--account",
            LOGIN_NAME,
            "--credential-store",
            "file",
        ])
        .assert()
        .success();
    // Pending, then slow-down: the device grant interval is honored, never shortened.
    assert!(begin.elapsed() >= Duration::from_secs(20));
    assert_eq!(json_stdout(&login)["data"]["authenticated"], true);
    let combined = format!("{}{}", stdout(&login), stderr(&login));
    for secret in [
        "device-secret",
        "login-access-secret",
        "login-refresh-secret",
    ] {
        assert!(!combined.contains(secret));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn denied_and_expired_device_requests_never_save_credentials() {
    for (error, exit) in [("access_denied", 3), ("expired_token", 5)] {
        let (server, dir) = fixture().await;
        respond_once(
            &server,
            route("POST", "/oauth/device_authorization"),
            ok(json!({
                "device_code":"secret-device","user_code":"ABCD-EFGH",
                "verification_uri":"https://app.voltage.cloud/cli/authorize","expires_in":60,"interval":5
            })),
        )
        .await;
        respond_once(
            &server,
            route("POST", "/oauth/token"),
            ResponseTemplate::new(400).set_body_json(json!({"error":error})),
        )
        .await;
        cli(dir.path(), &server)
            .args([
                "login",
                "--auth-url",
                &server.uri(),
                "--no-browser",
                "--credential-store",
                "file",
            ])
            .assert()
            .code(exit);
        assert!(!dir.path().join("config.toml").exists());
        assert!(!dir.path().join("credentials").exists());
        server.verify().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_processes_refresh_once_and_save_the_rotated_token() {
    let (server, dir) = fixture().await;
    let settings = seeded_login(dir.path(), &server, true);
    respond_once(
        &server,
        route("POST", "/oauth/token").and(body_string_contains(format!(
            "refresh_token={LOGIN_REFRESH}"
        ))),
        ok(json!({
            "access_token":"new-access","refresh_token":"new-refresh",
            "token_type":"Bearer","expires_in":600
        }))
        .set_delay(Duration::from_millis(100)),
    )
    .await;
    organization_exchange(&server, "new-access", ORG, "org-access", 2).await;
    respond(
        &server,
        route("GET", wallets_path(ORG)).and(header("authorization", "Bearer org-access")),
        ok(json!([])),
        2,
    )
    .await;
    let command = || {
        let mut cmd = cli(dir.path(), &server);
        cmd.args(["wallets", "list", "--account", LOGIN_NAME, "--org", ORG]);
        cmd
    };
    let mut a = command();
    let mut b = command();
    let (a, b) = tokio::join!(
        tokio::task::spawn_blocking(move || a.assert().success()),
        tokio::task::spawn_blocking(move || b.assert().success())
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(
        login_tokens(&settings, LOGIN_NAME),
        ("new-access".to_owned(), "new-refresh".to_owned())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn organization_exchange_uses_the_saved_auth_origin_and_keeps_discovery_on_login() {
    let auth = MockServer::start().await;
    let (api, dir) = fixture().await;
    let settings = seeded_login(dir.path(), &auth, false);
    respond_once(
        &auth,
        route("GET", "/users/current")
            .and(header("authorization", format!("Bearer {LOGIN_ACCESS}"))),
        ok(json!({"organizations": [{"id": ORG}]})),
    )
    .await;
    cli(dir.path(), &api)
        .args(["organizations", "list", "--account", LOGIN_NAME])
        .assert()
        .success();
    for (org, token) in [(ORG, "first-org-access"), (RESOURCE, "second-org-access")] {
        organization_exchange(&auth, LOGIN_ACCESS, org, token, 1).await;
        respond_once(
            &api,
            route("GET", wallets_path(org)).and(header("authorization", format!("Bearer {token}"))),
            ok(json!([])),
        )
        .await;
        cli(dir.path(), &api)
            .args(["wallets", "list", "--account", LOGIN_NAME, "--org", org])
            .assert()
            .success();
    }
    organization_exchange(&auth, LOGIN_ACCESS, ORG, "environment-access", 1).await;
    respond_once(
        &auth,
        route("GET", format!("/organizations/{ORG}/environments"))
            .and(header("authorization", "Bearer environment-access")),
        ok(json!([])),
    )
    .await;
    cli(dir.path(), &api)
        .args([
            "environments",
            "list",
            "--account",
            LOGIN_NAME,
            "--org",
            ORG,
        ])
        .assert()
        .success();
    assert_eq!(
        login_tokens(&settings, LOGIN_NAME),
        (LOGIN_ACCESS.to_owned(), LOGIN_REFRESH.to_owned())
    );
    assert!(
        api.received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| {
                !request.headers.contains_key("x-api-key") && request.url.path() != "/oauth/token"
            })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_or_invalid_exchanges_never_fall_back_to_the_login_token() {
    for (status, body) in [
        (400, json!({"error": "invalid_target"})),
        (
            200,
            json!({"access_token":"bad-access","token_type":"Bearer","expires_in":600}),
        ),
        (
            200,
            json!({"access_token":"bad-access","issued_token_type":"urn:ietf:params:oauth:token-type:access_token","token_type":"Bearer","expires_in":0}),
        ),
    ] {
        let auth = MockServer::start().await;
        let (api, dir) = fixture().await;
        let settings = seeded_login(dir.path(), &auth, false);
        respond_once(
            &auth,
            route("POST", "/oauth/token"),
            ResponseTemplate::new(status).set_body_json(body),
        )
        .await;
        let result = cli(dir.path(), &api)
            .args(["wallets", "list", "--account", LOGIN_NAME, "--org", ORG])
            .assert()
            .code(3);
        let output = stdout(&result);
        assert!(
            !output.contains(LOGIN_ACCESS)
                && !output.contains(LOGIN_REFRESH)
                && !output.contains("bad-access")
        );
        assert!(api.received_requests().await.unwrap().is_empty());
        assert_eq!(login_tokens(&settings, LOGIN_NAME).0, LOGIN_ACCESS);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logout_retains_credentials_on_revocation_failure_and_local_is_explicit() {
    let (server, dir) = fixture().await;
    let settings = seeded_login(dir.path(), &server, false);
    respond_once(
        &server,
        route("POST", "/oauth/revoke"),
        ResponseTemplate::new(503).set_body_json(json!({"error":"unavailable"})),
    )
    .await;
    cli(dir.path(), &server)
        .args(["logout", "--account", LOGIN_NAME])
        .assert()
        .code(3);
    assert!(settings.read_credential(LOGIN_NAME).is_ok());
    cli(dir.path(), &server)
        .args(["logout", "--account", LOGIN_NAME, "--local"])
        .assert()
        .success();
    assert!(settings.read_credential(LOGIN_NAME).is_err());
}

// ---------------------------------------------------------------------------------------
// Payments: submission, confirmation, waits, and reconciliation
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn raw_send_without_yes_never_submits() {
    let (server, dir) = fixture().await;
    let original = json!({"id":RESOURCE,"wallet_id":WALLET,"currency":"btc","type":"bolt11","data":{"payment_request":"invoice"}});
    for kind in [
        None,
        Some(Value::Null),
        Some(json!("bolt11")),
        Some(json!("unknown")),
    ] {
        let mut body = original.clone();
        if let Some(kind) = kind {
            body["payment_kind"] = kind;
        }
        let result = cli(dir.path(), &server)
            .args([
                "payments", "create", "--org", ORG, "--env", ENV, "--data", "-",
            ])
            .write_stdin(body.to_string())
            .assert()
            .code(2);
        assert!(stderr(&result).contains("requires --yes"));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(!dir.path().join("requests").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn payment_timeout_preserves_id_and_does_not_retry() {
    let (server, dir) = fixture().await;
    respond_once(
        &server,
        Mock::given(method("POST")),
        accepted().set_delay(Duration::from_secs(3)),
    )
    .await;
    let body = json!({"id":RESOURCE,"wallet_id":WALLET,"currency":"btc","type":"bolt11","data":{"payment_request":"invoice"}});
    let result = cli(dir.path(), &server)
        .args([
            "payments",
            "create",
            "--org",
            ORG,
            "--env",
            ENV,
            "--data",
            "-",
            "--yes",
            "--timeout",
            "1",
        ])
        .write_stdin(body.to_string())
        .assert()
        .code(4);
    let message = stderr(&result);
    assert!(message.contains(RESOURCE));
    assert!(message.contains("a write may have been submitted. No mutation was retried."));
    assert!(
        dir.path()
            .join(format!("requests/{RESOURCE}.json"))
            .exists()
    );
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ambiguous_payment_is_reconciled_by_reading_the_same_id_without_resubmission() {
    let (server, dir) = fixture().await;
    respond_once(
        &server,
        Mock::given(method("POST")),
        accepted().set_delay(Duration::from_secs(3)),
    )
    .await;
    respond_once(
        &server,
        route("GET", payment_path()),
        ok(json!({"id":RESOURCE,"status":"generating"})),
    )
    .await;
    let result = cli(dir.path(), &server)
        .args(receive_args(&["--wallet", WALLET, "--timeout", "1"]))
        .assert()
        .success();
    assert_eq!(json_stdout(&result)["outcome"], "accepted");
    assert_eq!(json_stdout(&result)["resource_id"], RESOURCE);
    assert!(stderr(&result).contains("reconciled"));
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_for_a_payment_tolerates_an_initial_missing_projection() {
    let (server, dir) = fixture().await;
    let not_found = ResponseTemplate::new(404).set_body_json(json!({"error":"not_found"}));
    Mock::given(method("GET"))
        .respond_with(in_order(vec![
            not_found.clone(),
            not_found,
            ok(json!({"id":RESOURCE,"status":"completed"})),
        ]))
        .expect(3)
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args([
            "payments",
            "get",
            RESOURCE,
            "--org",
            ORG,
            "--env",
            ENV,
            "--wait",
            "completed",
            "--timeout",
            "5",
        ])
        .assert()
        .success();
    assert_eq!(json_stdout(&result)["outcome"], "completed");
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_payment_reads_fail_normally_and_explicit_waits_time_out() {
    let (server, dir) = fixture().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error":"not_found"})))
        .mount(&server)
        .await;
    cli(dir.path(), &server)
        .args(["payments", "get", RESOURCE, "--org", ORG, "--env", ENV])
        .assert()
        .code(1);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    let result = cli(dir.path(), &server)
        .args([
            "payments",
            "get",
            RESOURCE,
            "--org",
            ORG,
            "--env",
            ENV,
            "--wait",
            "ready",
            "--timeout",
            "2",
        ])
        .assert()
        .code(5);
    assert!(stderr(&result).contains(RESOURCE));
    assert!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.method.as_str() == "GET")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_timeout_retains_the_accepted_payment_id() {
    let (server, dir) = fixture().await;
    respond_once(&server, Mock::given(method("POST")), accepted()).await;
    Mock::given(method("GET"))
        .respond_with(ok(json!({"id":RESOURCE,"status":"receiving"})))
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args(receive_args(&[
            "--wallet",
            WALLET,
            "--wait",
            "completed",
            "--timeout",
            "2",
        ]))
        .assert()
        .code(5);
    assert!(stderr(&result).contains(RESOURCE));
    assert!(
        dir.path()
            .join(format!("requests/{RESOURCE}.json"))
            .exists()
    );
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn qr_implies_invoice_readiness_wait() {
    let (server, dir) = fixture().await;
    respond_once(&server, route("POST", payments_path()), accepted()).await;
    respond_once(
        &server,
        route("GET", payment_path()),
        ok(json!({
            "id": RESOURCE,
            "direction": "receive",
            "type": "bolt11",
            "status": "receiving",
            "data": {"payment_request": "lntbs1example"}
        })),
    )
    .await;
    let result = cli(dir.path(), &server)
        .env("VOLTAGE_WALLET_ID", WALLET)
        .args(receive_args(&["--amount", "1", "--unit", "sats", "--qr"]))
        .assert()
        .success();
    assert_eq!(json_stdout(&result)["outcome"], "ready");
    let diagnostics = stderr(&result);
    assert!(diagnostics.contains(&format!("Payment {RESOURCE} status: receiving")));
    assert!(diagnostics.contains("Scan to pay:"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_tolerates_projection_delay_and_distinguishes_invoice_from_settlement() {
    let (server, dir) = fixture().await;
    respond_once(&server, route("POST", payments_path()), accepted()).await;
    route("GET", payment_path())
        .respond_with(in_order(vec![
            ResponseTemplate::new(404).set_body_json(json!({"error":"not_found"})),
            ok(json!({"id":RESOURCE,"status":"receiving","data":{"payment_request":"invoice"}})),
            ok(json!({"id":RESOURCE,"status":"completed"})),
        ]))
        .expect(3)
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .env("VOLTAGE_WALLET_ID", WALLET)
        .args(receive_args(&[
            "--amount",
            "1",
            "--unit",
            "sats",
            "--wait",
            "completed",
        ]))
        .assert()
        .success();
    assert_eq!(json_stdout(&result)["outcome"], "completed");
}

// ---------------------------------------------------------------------------------------
// Pagination, output, and transport failures
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cursor_pagination_is_the_default_and_carries_filters_across_pages() {
    let (server, dir) = fixture().await;
    let filtered = || {
        route("GET", payments_path())
            .and(query_param("pagination", "cursor"))
            .and(query_param("statuses[]", "completed"))
            .and(query_param("limit", "1"))
    };
    respond_once(
        &server,
        filtered().and(query_param_is_missing("cursor")),
        ok(json!({"items":[{"id":RESOURCE}],"offset":0,"limit":1,"next_cursor":"page-two","has_more":true})),
    )
    .await;
    respond_once(
        &server,
        filtered().and(query_param("cursor", "page-two")),
        ok(json!({"items":[{"id":WALLET}],"offset":0,"limit":1,"next_cursor":null,"has_more":false})),
    )
    .await;
    let result = cli(dir.path(), &server)
        .args([
            "payments",
            "list",
            "--org",
            ORG,
            "--env",
            ENV,
            "--statuses",
            "completed",
            "--limit",
            "1",
            "--all",
        ])
        .assert()
        .success();
    let pages = json_stdout(&result)["data"]["pages"].clone();
    assert_eq!(pages.as_array().unwrap().len(), 2);
    assert_eq!(pages[1]["data"]["items"][0]["id"], WALLET);
    assert!(!stderr(&result).contains("deprecated"));
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offset_only_endpoints_page_by_total_count() {
    let (server, dir) = fixture().await;
    let quotes = format!("/organizations/{ORG}/environments/{ENV}/quotes");
    respond_once(
        &server,
        route("GET", quotes.clone())
            .and(query_param("limit", "1"))
            .and(query_param_is_missing("offset")),
        ok(json!({"items":[{"id":RESOURCE}],"offset":0,"limit":1,"total":2})),
    )
    .await;
    respond_once(
        &server,
        route("GET", quotes)
            .and(query_param("limit", "1"))
            .and(query_param("offset", "1")),
        ok(json!({"items":[{"id":WALLET}],"offset":1,"limit":1,"total":2})),
    )
    .await;
    let result = cli(dir.path(), &server)
        .args([
            "quotes", "list", "--org", ORG, "--env", ENV, "--limit", "1", "--all",
        ])
        .assert()
        .success();
    assert_eq!(
        json_stdout(&result)["data"]["pages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_offset_paging_on_cursor_endpoints_warns_and_still_follows_has_more() {
    let (server, dir) = fixture().await;
    for (offset, more) in [("0", true), ("1", false)] {
        respond_once(
            &server,
            route("GET", payments_path())
                .and(query_param("offset", offset))
                .and(query_param("pagination", "offset"))
                .and(query_param("limit", "1")),
            ok(json!({
                "items":[{"id":RESOURCE}],"offset":offset.parse::<u64>().unwrap(),
                "limit":1,"has_more":more,"next_cursor":null
            })),
        )
        .await;
    }
    let result = cli(dir.path(), &server)
        .args([
            "payments",
            "list",
            "--org",
            ORG,
            "--env",
            ENV,
            "--offset",
            "0",
            "--pagination",
            "offset",
            "--limit",
            "1",
            "--all",
        ])
        .assert()
        .success();
    assert_eq!(
        json_stdout(&result)["data"]["pages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(stderr(&result).contains("Offset pagination is deprecated"));
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_destination_is_required_before_request() {
    let (server, dir) = fixture().await;
    cli(dir.path(), &server)
        .args([
            "webhooks", "keys", "rotate", RESOURCE, "--org", ORG, "--env", ENV, "--yes",
        ])
        .assert()
        .code(2);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn file_output_retains_one_time_secret_with_private_permissions() {
    let (server, dir) = fixture().await;
    let out = dir.path().join("secret.json");
    respond_once(
        &server,
        Mock::given(method("POST")),
        accepted().set_body_json(json!({"id":RESOURCE,"shared_secret":"one-time"})),
    )
    .await;
    let result = cli(dir.path(), &server)
        .args([
            "webhooks",
            "keys",
            "rotate",
            RESOURCE,
            "--org",
            ORG,
            "--env",
            ENV,
            "--yes",
            "--output-file",
        ])
        .arg(&out)
        .assert()
        .success();
    assert!(result.get_output().stdout.is_empty());
    assert!(config::read_private(&out).unwrap().contains("one-time"));
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_errors_redact_echoed_credentials() {
    let (server, dir) = fixture().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({"message":format!("Bad key {ACCOUNT_KEY}")})),
        )
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .code(3);
    assert!(!stderr(&result).contains(ACCOUNT_KEY));
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_wallet_reads_do_not_report_uncertain_writes() {
    use std::io::{Read, Write};
    for partial_response in [false, true] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            assert!(request.starts_with(b"GET "));
            if partial_response {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{\"token\":\"private-response-token\"").unwrap();
            }
            // Disconnect either before headers or partway through the body.
        });
        let dir = private_tempdir();
        let result = cli_at(dir.path(), &format!("http://{address}"))
            .args(["wallets", "list", "--org", ORG, "--env", ENV])
            .assert()
            .code(4);
        let message = stderr(&result);
        assert!(message.contains("HTTP read"), "{message}");
        assert!(!message.contains("write"), "{message}");
        assert!(!message.contains("mutation"), "{message}");
        assert!(!message.contains(ACCOUNT_KEY));
        assert!(!message.contains("private-response-token"));
        peer.join().unwrap();
    }
}

// ---------------------------------------------------------------------------------------
// Checkout credentials and streams
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn checkout_session_retries_projection_and_never_uses_account_key() {
    let (server, dir) = fixture().await;
    Mock::given(method("GET"))
        .respond_with(in_order(vec![
            accepted()
                .insert_header("x-retry-after-ms", "1")
                .set_body_json(json!({"id":RESOURCE})),
            ok(json!({"id":RESOURCE,"status":"ready"})),
        ]))
        .expect(2)
        .mount(&server)
        .await;
    cli(dir.path(), &server)
        .env("VOLTAGE_CHECKOUT_TOKEN", CHECKOUT_KEY)
        .args(["checkout", "sessions", "get", RESOURCE])
        .assert()
        .success();
    for request in server.received_requests().await.unwrap() {
        assert!(request.headers.get("x-api-key").is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn checkout_tokens_come_from_private_files_or_stdin_never_the_account_key() {
    let (server, dir) = fixture().await;
    let session = format!("/checkout/sessions/{RESOURCE}");
    for token in ["file-token", "stdin-token"] {
        respond_once(
            &server,
            route("GET", session.clone()).and(header("authorization", format!("Bearer {token}"))),
            ok(json!({"id":RESOURCE,"status":"ready"})),
        )
        .await;
    }
    let token_file = dir.path().join("checkout.token");
    std::fs::write(&token_file, "file-token\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    cli(dir.path(), &server)
        .args(["checkout", "sessions", "get", RESOURCE, "--token-file"])
        .arg(&token_file)
        .assert()
        .success();
    cli(dir.path(), &server)
        .args(["checkout", "sessions", "get", RESOURCE, "--token-file", "-"])
        .write_stdin("stdin-token\n")
        .assert()
        .success();
    let missing = cli(dir.path(), &server)
        .env_remove("VOLTAGE_CHECKOUT_TOKEN")
        .args(["checkout", "sessions", "get", RESOURCE])
        .assert()
        .code(3);
    assert!(stderr(&missing).contains("VOLTAGE_CHECKOUT_TOKEN"));
    for request in server.received_requests().await.unwrap() {
        assert!(request.headers.get("x-api-key").is_none());
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupting_checkout_stream_returns_130_without_leaking_its_token() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Command as Process, Stdio};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let dir = private_tempdir();
    let mut child = Process::new(assert_cmd::cargo::cargo_bin!("voltage"))
        .args([
            "checkout",
            "events",
            "watch",
            "--api-url",
            &format!("http://{address}"),
            "--origin",
            "https://shop.example.test",
        ])
        .arg("--config-dir")
        .arg(dir.path())
        .env("VOLTAGE_STREAM_TOKEN", "private-stream-token")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut socket, _) = tokio::task::spawn_blocking(move || listener.accept().unwrap())
        .await
        .unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
    }
    assert!(String::from_utf8_lossy(&request).contains("origin: https://shop.example.test"));
    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"connected\":true}\n\n").unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut event = String::new();
    reader.read_line(&mut event).unwrap();
    assert!(event.contains("connected"));
    // The socket remains open; SIGINT must cancel the event reader promptly.
    Process::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-stream-token"));
}

// ---------------------------------------------------------------------------------------
// Prices and conversions
// ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn conversions_show_sats_and_carry_the_quote_they_used() {
    let (server, dir) = fixture().await;
    respond_once(
        &server,
        route("GET", "/currency/BTCUSD/now"),
        ok(json!({"pair":"BTCUSD","time":"2026-09-18T17:30:00Z","price":"80000.50"})),
    )
    .await;
    respond_once(
        &server,
        route("GET", "/currency/BTCUSD/2026-09-01T00:00:00Z"),
        ok(json!({"pair":"BTCUSD","time":"2026-09-01T00:00:00Z","price":"50000"})),
    )
    .await;
    let usd = cli(dir.path(), &server)
        .args([
            "convert",
            "10",
            "usd",
            "--to",
            "btc",
            "--price-url",
            &server.uri(),
        ])
        .assert()
        .success();
    assert_eq!(
        json_stdout(&usd)["data"],
        json!({
            "from": {"currency":"usd","amount":1000},
            "to": {"currency":"btc","msats":12499922,"sats":"12499.922","btc":"0.00012499922"},
            "price": {"pair":"BTCUSD","time":"2026-09-18T17:30:00Z","price":"80000.50"}
        })
    );
    let sats = cli(dir.path(), &server)
        .args([
            "convert",
            "21000",
            "sats",
            "--to",
            "usd",
            "--at",
            "2026-09-01T00:00:00Z",
            "--price-url",
            &server.uri(),
        ])
        .assert()
        .success();
    assert_eq!(
        json_stdout(&sats)["data"]["to"],
        json!({"currency":"usd","cents":1050,"usd":"10.5"})
    );
    cli(dir.path(), &server)
        .args([
            "convert",
            "10",
            "usd",
            "--to",
            "usd",
            "--price-url",
            &server.uri(),
        ])
        .assert()
        .code(2);
    cli(dir.path(), &server)
        .args([
            "convert",
            "10",
            "usd",
            "--to",
            "btc",
            "--at",
            "yesterday",
            "--price-url",
            &server.uri(),
        ])
        .assert()
        .code(2);
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn price_reports_sats_per_dollar_and_surfaces_service_rejections() {
    let (server, dir) = fixture().await;
    respond_once(
        &server,
        route("GET", "/currency/BTCUSD/now"),
        ok(json!({"pair":"BTCUSD","time":"2026-09-18T17:30:00Z","price":"80727.836666666667"})),
    )
    .await;
    let price = cli(dir.path(), &server)
        .args(["price", "--price-url", &server.uri()])
        .assert()
        .success();
    assert_eq!(
        json_stdout(&price)["data"],
        json!({"pair":"BTCUSD","time":"2026-09-18T17:30:00Z","price":"80727.836666666667","sats_per_usd":"1238.73"})
    );
    let (rejecting, dir) = fixture().await;
    respond_once(
        &rejecting,
        route("GET", "/currency/BTCUSD/now"),
        ResponseTemplate::new(400)
            .set_body_json(json!({"error":"Unsupported currency pair: BTCEUR"})),
    )
    .await;
    let rejected = cli(dir.path(), &rejecting)
        .args(["price", "--price-url", &rejecting.uri()])
        .assert()
        .code(1);
    assert!(stderr(&rejected).contains("Unsupported currency pair"));
}
