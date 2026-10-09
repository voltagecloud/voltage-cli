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
#[cfg(unix)]
use tokio::sync::Notify;
use voltage_cli::{
    config::{
        self, Account, AccountKind, Config, Credential, CredentialStore, Login, Profile, Settings,
    },
    registry::{AuthScheme, Method, OPERATIONS, OperationId, ResourceTarget},
    secret::Secret,
};
use wiremock::{
    Mock, MockBuilder, MockServer, Request, Respond, ResponseTemplate,
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
    Command::from_std(process_at(dir, api_url))
}

/// `cli_at` as a plain process, for tests that attach a terminal or deliver a signal.
fn process_at(dir: &Path, api_url: &str) -> std::process::Command {
    let mut cmd = human_process_at(dir, api_url);
    cmd.arg("--json");
    cmd
}

/// `process_at` without `--json`, so the output format can be chosen.
fn human_process_at(dir: &Path, api_url: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin!("voltage"));
    for name in [
        "VOLTAGE_ORGANIZATION_ID",
        "VOLTAGE_ENVIRONMENT_ID",
        "VOLTAGE_WALLET_ID",
        "VOLTAGE_API_KEY",
        "VOLTAGE_CHECKOUT_TOKEN",
        "VOLTAGE_STREAM_TOKEN",
        "VOLTAGE_CONFIG_DIR",
        "VOLTAGE_EXECUTE",
    ] {
        cmd.env_remove(name);
    }
    // Most tests exercise sending, as a script that exports VOLTAGE_EXECUTE does; the
    // describe-by-default tests remove it.
    cmd.env("VOLTAGE_API_KEY", ACCOUNT_KEY)
        .env("VOLTAGE_EXECUTE", "1")
        .arg("--config-dir")
        .arg(dir)
        .arg("--api-url")
        .arg(api_url);
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
    response: impl Respond + 'static,
    times: u64,
) {
    request
        .respond_with(response)
        .expect(times)
        .mount(server)
        .await;
}

async fn respond_once(server: &MockServer, request: MockBuilder, response: impl Respond + 'static) {
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
        .env("VOLTAGE_EXECUTE", "1")
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
            .args(["--org", ORG, "--env", ENV, "--show-secrets"]);
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

#[tokio::test]
async fn context_reports_effective_scope_and_sources_without_reading_secrets() {
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
    // Inspection must not need the secret, so remove it from the store.
    for entry in std::fs::read_dir(dir.path().join("credentials")).unwrap() {
        std::fs::remove_file(entry.unwrap().path()).unwrap();
    }
    let profiled = cli(dir.path(), &server)
        .env("VOLTAGE_ORGANIZATION_ID", RESOURCE)
        .env("VOLTAGE_CONFIG_DIR", dir.path().join("unused"))
        .args(["context", "--profile", "stage", "--wallet", WALLET])
        .assert()
        .success();
    let setting = |value: Value, source: &str| json!({"value": value, "source": source});
    let data = json_stdout(&profiled)["data"].clone();
    assert_eq!(data["profile"], setting(json!("stage"), "flag"));
    assert_eq!(data["account"], setting(json!("stage"), "profile"));
    assert_eq!(
        data["credential"],
        json!({"kind": "api_key", "location": "file"})
    );
    assert_eq!(data["credential_problem"], Value::Null);
    assert_eq!(data["organization_id"], setting(json!(ORG), "profile"));
    assert_eq!(data["environment_ids"], setting(json!([ENV]), "profile"));
    assert_eq!(data["wallet_id"], setting(json!(WALLET), "flag"));
    assert_eq!(data["webhook_id"], setting(Value::Null, "unset"));
    assert_eq!(data["api_url"], setting(json!(server.uri()), "flag"));
    assert_eq!(
        data["config_dir"],
        setting(json!(dir.path().display().to_string()), "flag")
    );
    assert_eq!(data["config_file"]["exists"], true);
    assert_eq!(data["execute_changes"], setting(json!(true), "environment"));
    // The profile silences ambient scope and the ambient key; the flag beats the variable.
    assert_eq!(
        data["ignored_variables"],
        json!([
            "VOLTAGE_ORGANIZATION_ID",
            "VOLTAGE_CONFIG_DIR",
            "VOLTAGE_API_KEY"
        ])
    );
    assert!(!stdout(&profiled).contains("stage-key"));

    let ambient = cli(dir.path(), &server)
        .env("VOLTAGE_ORGANIZATION_ID", ORG)
        .args(["context", "--env", ENV])
        .assert()
        .success();
    let data = json_stdout(&ambient)["data"].clone();
    assert_eq!(data["account"], setting(Value::Null, "environment"));
    assert_eq!(
        data["credential"],
        json!({"kind": "api_key", "location": "environment"})
    );
    assert_eq!(data["credential_problem"], Value::Null);
    assert_eq!(data["organization_id"], setting(json!(ORG), "environment"));
    assert_eq!(data["environment_ids"], setting(json!([ENV]), "flag"));
    assert_eq!(data["ignored_variables"], json!([]));
    assert!(!stdout(&ambient).contains(ACCOUNT_KEY));

    // A blank key still wins over saved credentials, and every command rejects it.
    let blank = cli(dir.path(), &server)
        .env("VOLTAGE_API_KEY", " ")
        .arg("context")
        .assert()
        .success();
    let data = json_stdout(&blank)["data"].clone();
    assert_eq!(
        data["credential"],
        json!({"kind": "api_key", "location": "environment"})
    );
    assert_eq!(data["credential_problem"], "empty_api_key");

    let only_saved = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .env_remove("VOLTAGE_EXECUTE")
        .arg("context")
        .assert()
        .success();
    let data = json_stdout(&only_saved)["data"].clone();
    assert_eq!(data["account"], setting(json!("stage"), "default"));
    assert_eq!(data["execute_changes"], setting(json!(false), "default"));

    // A saved key used outside its bound organization is reported the way commands reject it.
    let elsewhere = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(["context", "--org", RESOURCE])
        .assert()
        .success();
    let data = json_stdout(&elsewhere)["data"].clone();
    assert_eq!(
        data["credential"],
        json!({"kind": "api_key", "location": "file"})
    );
    assert_eq!(data["credential_problem"], "bound_elsewhere");
    cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(["wallets", "list", "--org", RESOURCE])
        .assert()
        .code(2);

    let unknown = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(["context", "--account", "missing"])
        .assert()
        .success();
    let data = json_stdout(&unknown)["data"].clone();
    assert_eq!(data["account"], setting(json!("missing"), "flag"));
    assert_eq!(data["credential"], Value::Null);
    assert_eq!(data["credential_problem"], "unknown_credential");

    let stage = settings.config.accounts["stage"].clone();
    settings.config.accounts.insert("prod".into(), stage);
    settings.save().unwrap();
    let ambiguous = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .arg("context")
        .assert()
        .success();
    let data = json_stdout(&ambiguous)["data"].clone();
    assert_eq!(data["account"], setting(Value::Null, "unset"));
    assert_eq!(data["credential_problem"], "multiple_credentials");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn context_inspection_creates_no_configuration() {
    let (server, dir) = fixture().await;
    let fresh = dir.path().join("fresh");
    let inspected = cli(&fresh, &server)
        .env_remove("VOLTAGE_API_KEY")
        .arg("context")
        .assert()
        .success();
    let data = json_stdout(&inspected)["data"].clone();
    assert_eq!(data["account"], json!({"value": null, "source": "unset"}));
    assert_eq!(data["credential"], Value::Null);
    assert_eq!(data["config_file"]["exists"], false);
    assert!(!fresh.exists());
    cli(&fresh, &server)
        .args(["context", "--profile", "missing"])
        .assert()
        .code(2);
    assert!(!fresh.exists());
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_login_does_not_wait_for_the_credential_lock() {
    use std::os::unix::fs::OpenOptionsExt;
    let (server, dir) = fixture().await;
    seeded_login(dir.path(), &server, false);
    organization_exchange(&server, LOGIN_ACCESS, ORG, "org-access", 1).await;
    respond_once(
        &server,
        route("GET", wallets_path(ORG)).and(header("authorization", "Bearer org-access")),
        ok(json!([])),
    )
    .await;
    // Another process holds the lock, as it would during a slow refresh.
    let held = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(dir.path().join("credentials.lock"))
        .unwrap();
    held.lock().unwrap();
    let mut command = cli(dir.path(), &server);
    command
        .args(["wallets", "list", "--account", LOGIN_NAME, "--org", ORG])
        .timeout(Duration::from_secs(10));
    tokio::task::spawn_blocking(move || command.assert().success())
        .await
        .unwrap();
    drop(held);
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

/// A pseudo-terminal pair: the controller end reads what the child writes, and the terminal
/// end is handed to the child as stdin or stderr.
#[cfg(unix)]
fn open_pty() -> (std::fs::File, std::fs::File) {
    use std::os::fd::FromRawFd;
    let mut controller = 0;
    let mut terminal = 0;
    // SAFETY: openpty only writes the two descriptors on success; the name, termios, and
    // window-size pointers are optional and may be null.
    let opened = unsafe {
        libc::openpty(
            &mut controller,
            &mut terminal,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(opened, 0, "{}", std::io::Error::last_os_error());
    // openpty has no close-on-exec option. Without it, a child that another test spawns
    // concurrently inherits both ends and keeps the controller's drain open after this test's
    // child exits. The descriptor a child receives as stdio is a dup2 copy, which does not
    // keep the flag. Only a spawn between openpty and these calls can still inherit them.
    for descriptor in [controller, terminal] {
        // SAFETY: the descriptor was just opened, and F_SETFD only changes its flags.
        let set = unsafe { libc::fcntl(descriptor, libc::F_SETFD, libc::FD_CLOEXEC) };
        assert_eq!(set, 0, "{}", std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just opened, nothing else owns them, and each is wrapped
    // exactly once, so each `File` closes its descriptor once.
    unsafe {
        (
            std::fs::File::from_raw_fd(controller),
            std::fs::File::from_raw_fd(terminal),
        )
    }
}

/// Deliver SIGINT, as Ctrl-C does on a terminal.
#[cfg(unix)]
fn interrupt(child: &std::process::Child) {
    let status = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

/// Answer with `response` and announce each arrival, so a test acts only once the request
/// is in flight.
#[cfg(unix)]
fn announced(response: ResponseTemplate) -> (impl Respond, Arc<Notify>) {
    let arrived = Arc::new(Notify::new());
    let announce = Arc::clone(&arrived);
    let responder = move |_: &Request| {
        announce.notify_one();
        response.clone()
    };
    (responder, arrived)
}

/// Start the binary with `args`, interrupt it once its request reaches the server, and
/// return its output after it exits with 130.
#[cfg(unix)]
async fn interrupt_in_flight(
    dir: &Path,
    server: &MockServer,
    arrived: &Notify,
    args: &[&str],
) -> std::process::Output {
    use std::process::Stdio;
    let child = process_at(dir, &server.uri())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Generous: a device login waits out its first poll interval before any request.
    tokio::time::timeout(Duration::from_secs(15), arrived.notified())
        .await
        .unwrap();
    interrupt(&child);
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
    output
}

/// The JSON error report: the last line of stderr, after any recovery notice.
#[cfg(unix)]
fn error_report(stderr: &[u8]) -> Value {
    serde_json::from_str(String::from_utf8_lossy(stderr).lines().last().unwrap()).unwrap()
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn progress_is_terminal_only_and_cleared_on_completion() {
    use std::io::Read;
    use std::process::Stdio;
    let (server, dir) = fixture().await;
    route("GET", wallets_path(ORG))
        .respond_with(ok(json!({"items":[]})).set_delay(Duration::from_millis(400)))
        .expect(2)
        .mount(&server)
        .await;
    let args = ["wallets", "list", "--org", ORG];
    let redirected = process_at(dir.path(), &server.uri())
        .args(args)
        .output()
        .unwrap();
    assert!(redirected.status.success());
    assert!(redirected.stderr.is_empty());

    let (mut controller, terminal) = open_pty();
    let child = process_at(dir.path(), &server.uri())
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::from(terminal))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let reader = tokio::task::spawn_blocking(move || {
        let mut shown = Vec::new();
        // Linux PTYs return EIO rather than EOF when the terminal end closes.
        let _ = controller.read_to_end(&mut shown);
        shown
    });
    let output = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    assert!(output.status.success());
    let shown = reader.await.unwrap();
    let shown = String::from_utf8_lossy(&shown);
    assert!(shown.contains("Waiting for API response..."), "{shown}");
    assert!(shown.ends_with("\r\u{1b}[2K"), "{shown}");
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_input_requires_a_secret_source_instead_of_a_prompt() {
    let (server, dir) = fixture().await;
    let missing_secret = cli(dir.path(), &server)
        .args([
            "--no-input",
            "auth",
            "import-key",
            "--account",
            "other",
            "--org",
            ORG,
            "--env",
            ENV,
        ])
        .assert()
        .code(2);
    assert!(stderr(&missing_secret).contains("--stdin"));
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(!dir.path().join("requests").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn quiet_keeps_results_errors_and_payment_recovery_id() {
    let (server, dir) = fixture().await;
    respond_once(&server, route("POST", payments_path()), accepted()).await;
    let body = json!({"id":RESOURCE,"wallet_id":WALLET,"currency":"btc","type":"bolt11","data":{"payment_request":"invoice"}});
    let result = cli(dir.path(), &server)
        .args([
            "-q",
            "--no-input",
            "payments",
            "create",
            "--org",
            ORG,
            "--env",
            ENV,
            "--data",
            "-",
        ])
        .write_stdin(body.to_string())
        .assert()
        .success();
    assert_eq!(json_stdout(&result)["resource_id"], RESOURCE);
    assert!(stderr(&result).contains("Recovery record:"));
    assert!(
        stderr(&result).contains("Network/provider fee limits exclude additional processing fees.")
    );
    assert!(!stderr(&result).contains("\u{1b}["));
    let error = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args([
            "--quiet",
            "--no-input",
            "payments",
            "create",
            "--org",
            ORG,
            "--env",
            ENV,
            "--data",
            "-",
        ])
        .write_stdin(body.to_string())
        .assert()
        .code(6);
    assert!(stderr(&error).contains("pass --execute"));
    server.verify().await;
}

/// Local modes of a pseudo-terminal, as the child sees them through `/dev/tty`.
#[cfg(unix)]
fn local_modes(pty: &std::fs::File) -> libc::tcflag_t {
    use std::os::fd::AsRawFd;
    let mut modes = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: the descriptor is open, and tcgetattr fully initializes `modes` on success.
    assert_eq!(
        unsafe { libc::tcgetattr(pty.as_raw_fd(), modes.as_mut_ptr()) },
        0
    );
    // SAFETY: tcgetattr returned 0 above.
    unsafe { modes.assume_init() }.c_lflag
}

/// How a test delivers Ctrl-C to a child on a pseudo-terminal.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum CtrlC {
    /// SIGINT from outside, as the terminal sends it to the foreground process.
    Signal,
    /// The character a person types, which a prompt with terminal signals off reads itself.
    Typed,
}

/// Interrupt `auth import-key` once its hidden prompt has turned off echo. Returns the exit
/// code, whether echo is on afterward, and everything the child wrote to the terminal.
#[cfg(unix)]
async fn interrupt_the_hidden_key_prompt(ctrl_c: CtrlC) -> (Option<i32>, bool, String) {
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let (server, dir) = fixture().await;
    let (mut controller, terminal) = open_pty();
    // The terminal end is revoked when the session leader exits; the controller end keeps
    // reporting the shared attributes.
    let observer = controller.try_clone().unwrap();
    let mut command = process_at(dir.path(), &server.uri());
    command
        .args([
            "auth",
            "import-key",
            "--account",
            "k",
            "--org",
            ORG,
            "--env",
            ENV,
        ])
        .stdin(Stdio::from(terminal.try_clone().unwrap()))
        .stderr(Stdio::from(terminal))
        .stdout(Stdio::piped());
    // SAFETY: setsid and ioctl are async-signal-safe, and the closure allocates nothing.
    // The new session takes the PTY on stdin as its controlling terminal, so `/dev/tty`
    // names it.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    // A session leader's exit waits for its terminal output to drain, so the controller is
    // read until the child closes the terminal, not only until the prompt appears.
    let (prompted, prompt) = tokio::sync::oneshot::channel();
    let drain = tokio::task::spawn_blocking(move || {
        let mut prompted = Some(prompted);
        let mut shown = Vec::new();
        let mut chunk = [0; 4096];
        while let Ok(read @ 1..) = controller.read(&mut chunk) {
            shown.extend_from_slice(&chunk[..read]);
            if String::from_utf8_lossy(&shown).contains("Environment API key:") {
                prompted.take().map(|prompted| prompted.send(()));
            }
        }
        String::from_utf8_lossy(&shown).into_owned()
    });
    tokio::time::timeout(Duration::from_secs(10), prompt)
        .await
        .unwrap()
        .unwrap();
    // The prompt is written before echo is turned off; interrupt only once the read is
    // hidden, or the test would pass without restoring anything.
    tokio::time::timeout(Duration::from_secs(10), async {
        while local_modes(&observer) & libc::ECHO != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    match ctrl_c {
        CtrlC::Signal => interrupt(&child),
        CtrlC::Typed => (&observer).write_all(b"\x03").unwrap(),
    }
    let pid = child.id().to_string();
    let exited = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await;
    let Ok(output) = exited else {
        std::process::Command::new("kill")
            .args(["-KILL", &pid])
            .status()
            .unwrap();
        panic!("Ctrl-C at the hidden key prompt did not end the process");
    };
    let code = output.unwrap().status.code();
    let echo = local_modes(&observer) & libc::ECHO != 0;
    // The controller reports end of input once the last terminal descriptor closes.
    drop(command);
    let shown = tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .unwrap()
        .unwrap();
    (code, echo, shown)
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_at_the_hidden_key_prompt_restores_terminal_echo() {
    let (code, echo, shown) = interrupt_the_hidden_key_prompt(CtrlC::Signal).await;
    assert_eq!(code, Some(130), "{shown}");
    assert!(echo);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn typed_ctrl_c_at_the_hidden_key_prompt_reports_an_interruption() {
    let (code, echo, shown) = interrupt_the_hidden_key_prompt(CtrlC::Typed).await;
    assert_eq!(code, Some(130), "{shown}");
    assert!(shown.contains("Interrupted before any resource change was submitted"));
    assert!(echo);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_during_a_refresh_says_the_login_may_need_renewal() {
    let (server, dir) = fixture().await;
    seeded_login(dir.path(), &server, true);
    let (responder, arrived) = announced(
        ok(json!({
            "access_token":"new-access","refresh_token":"new-refresh",
            "token_type":"Bearer","expires_in":600
        }))
        .set_delay(Duration::from_secs(10)),
    );
    respond_once(
        &server,
        route("POST", "/oauth/token").and(body_string_contains("grant_type=refresh_token")),
        responder,
    )
    .await;
    let output = interrupt_in_flight(
        dir.path(),
        &server,
        &arrived,
        &["wallets", "list", "--account", LOGIN_NAME, "--org", ORG],
    )
    .await;
    let report = error_report(&output.stderr);
    assert_eq!(
        report["error"]["message"],
        format!("Interrupted while refreshing the saved login for {LOGIN_NAME}")
    );
    assert!(
        report["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("voltage login")
    );
    server.verify().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_during_a_logout_revoke_points_to_a_local_logout() {
    let (server, dir) = fixture().await;
    let settings = seeded_login(dir.path(), &server, false);
    let (responder, arrived) = announced(ok(json!({})).set_delay(Duration::from_secs(10)));
    respond_once(&server, route("POST", "/oauth/revoke"), responder).await;
    let output = interrupt_in_flight(
        dir.path(),
        &server,
        &arrived,
        &["logout", "--account", LOGIN_NAME],
    )
    .await;
    let report = error_report(&output.stderr);
    assert_eq!(
        report["error"]["message"],
        format!("Interrupted while logging out {LOGIN_NAME}; its session may already be revoked")
    );
    assert!(
        report["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("--local")
    );
    // The saved login is still there for the local logout the hint suggests.
    assert!(settings.read_credential(LOGIN_NAME).is_ok());
    server.verify().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_before_a_new_login_is_saved_says_a_session_may_exist() {
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
    respond_once(
        &server,
        route("POST", "/oauth/token"),
        ok(json!({
            "access_token":"login-access","refresh_token":"login-refresh",
            "token_type":"Bearer","expires_in":600
        })),
    )
    .await;
    // The session exists once tokens are issued; saving it waits on the identity lookup.
    let (responder, arrived) = announced(
        ok(json!({"id":RESOURCE,"email":LOGIN_EMAIL})).set_delay(Duration::from_secs(10)),
    );
    respond_once(&server, route("GET", "/users/current"), responder).await;
    let output = interrupt_in_flight(
        dir.path(),
        &server,
        &arrived,
        &[
            "login",
            "--auth-url",
            &server.uri(),
            "--no-browser",
            "--account",
            LOGIN_NAME,
            "--credential-store",
            "file",
        ],
    )
    .await;
    let report = error_report(&output.stderr);
    assert_eq!(
        report["error"]["message"],
        "Interrupted after sign-in may have created a session that was not saved"
    );
    assert!(
        report["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("voltage login")
    );
    assert!(
        !settings_at(dir.path())
            .config
            .accounts
            .contains_key(LOGIN_NAME)
    );
    server.verify().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_during_a_read_does_not_claim_a_change_was_submitted() {
    let (server, dir) = fixture().await;
    let (responder, arrived) =
        announced(ok(json!({"items":[]})).set_delay(Duration::from_secs(10)));
    respond_once(&server, route("GET", wallets_path(ORG)), responder).await;
    let output = interrupt_in_flight(
        dir.path(),
        &server,
        &arrived,
        &["wallets", "list", "--org", ORG],
    )
    .await;
    let report = error_report(&output.stderr);
    assert_eq!(
        report["error"]["message"],
        "Interrupted before any resource change was submitted"
    );
    assert!(report["error"]["detail"].is_null());
    assert!(!dir.path().join("requests").exists());
    server.verify().await;
}

/// A `--data` read that is still waiting for input must not keep Ctrl-C from ending the
/// command. A FIFO blocks like an idle stdin, and opening its write end waits for the CLI to
/// open the read end, so the test knows the read has started.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_while_a_request_body_is_still_being_read() {
    use std::process::Stdio;
    let (server, dir) = fixture().await;
    let fifo = dir.path().join("body.json");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(made.success());
    let child = process_at(dir.path(), &server.uri())
        .args(["payments", "create", "--data"])
        .arg(format!("@{}", fifo.display()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Held open and never written, so the CLI's read blocks until the process ends.
    let _writer = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new().write(true).open(fifo).unwrap()
        }),
    )
    .await
    .unwrap()
    .unwrap();
    interrupt(&child);
    let pid = child.id().to_string();
    let Ok(output) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    else {
        std::process::Command::new("kill")
            .args(["-KILL", &pid])
            .status()
            .unwrap();
        panic!("Ctrl-C during a blocked --data read did not end the process");
    };
    let output = output.unwrap();
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        error_report(&output.stderr)["error"]["message"],
        "Interrupted before any resource change was submitted"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_during_payment_submission_reports_original_id_without_retry() {
    let (server, dir) = fixture().await;
    let (responder, arrived) = announced(accepted().set_delay(Duration::from_secs(10)));
    respond_once(&server, route("POST", payments_path()), responder).await;
    let body = json!({"id":RESOURCE,"wallet_id":WALLET,"currency":"btc","type":"bolt11","data":{"payment_request":"invoice"}});
    let data = dir.path().join("request.json");
    std::fs::write(&data, body.to_string()).unwrap();
    let data = format!("@{}", data.display());
    let output = interrupt_in_flight(
        dir.path(),
        &server,
        &arrived,
        &[
            "payments", "create", "--org", ORG, "--env", ENV, "--data", &data,
        ],
    )
    .await;
    assert!(output.stdout.is_empty());
    let report = error_report(&output.stderr);
    assert_eq!(report["error"]["detail"]["resource_id"], RESOURCE);
    assert_eq!(report["error"]["detail"]["outcome"], "unknown");
    let message = report["error"]["message"].as_str().unwrap();
    assert!(message.contains(&format!("payment {RESOURCE} may have been submitted")));
    assert!(message.contains("was not cancelled"));
    assert!(
        dir.path()
            .join(format!("requests/{RESOURCE}.json"))
            .exists()
    );
    server.verify().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_during_another_write_reports_an_uncertain_submission() {
    let (server, dir) = fixture().await;
    let (responder, arrived) =
        announced(ResponseTemplate::new(204).set_delay(Duration::from_secs(10)));
    respond_once(
        &server,
        route("DELETE", format!("{}/{WALLET}", wallets_path(ORG))),
        responder,
    )
    .await;
    let output = interrupt_in_flight(
        dir.path(),
        &server,
        &arrived,
        &["wallets", "delete", WALLET, "--org", ORG],
    )
    .await;
    let report = error_report(&output.stderr);
    let message = report["error"]["message"].as_str().unwrap();
    assert!(message.contains("may have been submitted"), "{message}");
    assert!(!message.contains("payment"), "{message}");
    assert_eq!(report["error"]["detail"]["organization_id"], ORG);
    assert_eq!(report["error"]["detail"]["outcome"], "unknown");
    server.verify().await;
}

#[tokio::test]
async fn dry_run_sends_nothing_records_nothing_and_omits_private_values() {
    let (server, dir) = fixture().await;
    let output_file = dir.path().join("result.json");
    let planned = cli(dir.path(), &server)
        .args([
            "payments",
            "send",
            "--org",
            ORG,
            "--env",
            ENV,
            "--wallet",
            WALLET,
            "--id",
            RESOURCE,
            "--currency",
            "btc",
            "--invoice",
            "lnbc1private",
            "--max-fee",
            "10",
            "--fee-unit",
            "sats",
            "--metadata",
            "order=private-order",
            "--dry-run",
        ])
        .arg("--output-file")
        .arg(&output_file)
        .assert()
        .success();
    let envelope = json_stdout(&planned);
    assert_eq!(envelope["outcome"], "dry_run");
    assert_eq!(envelope["http_status"], Value::Null);
    assert_eq!(envelope["resource_id"], RESOURCE);
    let data = &envelope["data"];
    assert_eq!(data["operation"], "payments send");
    assert_eq!(data["method"], "POST");
    assert_eq!(data["url"], format!("{}{}", server.uri(), payments_path()));
    assert_eq!(
        data["body"],
        json!({
            "id": RESOURCE, "wallet_id": WALLET, "type": "bolt11", "currency": "btc",
            "data": {"payment_request": "[OMITTED]", "max_fee": {"currency": "btc", "amount": 10000}},
            "metadata": "[OMITTED]"
        })
    );
    assert_eq!(data["values_omitted"], true);
    assert_eq!(
        data["run"],
        json!({
            "kind": "mutation",
            "verify_wallet_environment": false,
            "journal_id": RESOURCE,
            "reconcile": true,
            "wait": null
        })
    );
    let text = stdout(&planned);
    assert!(!text.contains("lnbc1private") && !text.contains("private-order"));
    assert!(!text.contains(ACCOUNT_KEY));
    assert!(!output_file.exists());
    assert!(!dir.path().join("requests").exists());

    // Wallet preflight reads are skipped when describing a delete.
    let delete = cli(dir.path(), &server)
        .args([
            "wallets", "delete", WALLET, "--org", ORG, "--env", ENV, "-n",
        ])
        .assert()
        .success();
    let data = json_stdout(&delete)["data"].clone();
    assert_eq!(data["method"], "DELETE");
    assert_eq!(data["run"]["verify_wallet_environment"], true);
    let read = cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG, "-n"])
        .assert()
        .success();
    assert_eq!(
        json_stdout(&read)["data"]["run"],
        json!({
            "kind": "read",
            "check_wallet_environment": false,
            "then": {"kind": "pages", "follow": false}
        })
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn changes_are_described_unless_execute_or_voltage_execute_asks_to_send_them() {
    let (server, dir) = fixture().await;
    let output_file = dir.path().join("result.json");
    let delete = ["wallets", "delete", WALLET, "--org", ORG];
    let described = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(delete)
        .arg("--output-file")
        .arg(&output_file)
        .assert()
        .code(6);
    let envelope = json_stdout(&described);
    assert_eq!(envelope["outcome"], "dry_run");
    assert_eq!(envelope["resource_id"], WALLET);
    assert!(stderr(&described).contains("pass --execute or set VOLTAGE_EXECUTE=1"));
    assert!(!output_file.exists());
    // A misspelled value is rejected for every API command, reads included, so a read-only
    // script surfaces it before its first change.
    let unclear = cli(dir.path(), &server)
        .env("VOLTAGE_EXECUTE", "yes")
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .code(2);
    assert!(stderr(&unclear).contains("VOLTAGE_EXECUTE must be 1, true, 0, or false"));
    // An explicit --dry-run describes and succeeds, even when the environment says to send.
    cli(dir.path(), &server)
        .args(delete)
        .arg("--dry-run")
        .assert()
        .success();
    assert!(server.received_requests().await.unwrap().is_empty());

    // Reads are unaffected by the default.
    respond_once(
        &server,
        route("GET", wallets_path(ORG)),
        ok(json!({"items": []})),
    )
    .await;
    cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .success();
    let path = format!("{}/{WALLET}", wallets_path(ORG));
    route("DELETE", path.as_str())
        .respond_with(ResponseTemplate::new(204))
        .expect(2)
        .mount(&server)
        .await;
    let flagged = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(delete)
        .arg("--execute")
        .assert()
        .success();
    assert!(!stderr(&flagged).contains("VOLTAGE_EXECUTE"));
    // Sending because of the environment is reported, so an exported variable is visible.
    let inherited = cli(dir.path(), &server).args(delete).assert().success();
    assert!(stderr(&inherited).contains("Sending this change because VOLTAGE_EXECUTE is set."));
    server.verify().await;
}

#[tokio::test]
async fn dry_run_reads_raw_bodies_and_shows_them_only_with_show_secrets() {
    let (server, dir) = fixture().await;
    let body = json!({"id":RESOURCE,"wallet_id":WALLET,"currency":"btc","type":"bolt11","data":{"payment_request":"lnbc1raw"},"extension":{"note":"x"}});
    let args = [
        "payments",
        "create",
        "--org",
        ORG,
        "--env",
        ENV,
        "--data",
        "-",
        "--dry-run",
    ];
    let hidden = cli(dir.path(), &server)
        .args(args)
        .write_stdin(body.to_string())
        .assert()
        .success();
    let data = json_stdout(&hidden)["data"].clone();
    assert_eq!(data["body"]["data"]["payment_request"], "[OMITTED]");
    assert_eq!(data["body"]["extension"], "[OMITTED]");
    assert!(data["notes"].as_array().unwrap().contains(&json!(
        "The --data payload was read and validated; a real run reads it again."
    )));
    let shown = cli(dir.path(), &server)
        .args(args)
        .arg("--show-secrets")
        .write_stdin(body.to_string())
        .assert()
        .success();
    let data = json_stdout(&shown)["data"].clone();
    assert_eq!(data["body"], body);
    assert_eq!(data["values_omitted"], false);
    // Local validation still applies.
    cli(dir.path(), &server)
        .args(args)
        .write_stdin(json!({"wallet_id": WALLET}).to_string())
        .assert()
        .code(2);
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(!dir.path().join("requests").exists());
}

#[tokio::test]
async fn dry_run_describes_checkout_requests_without_a_token() {
    let (server, dir) = fixture().await;
    let watched = cli(dir.path(), &server)
        .env_remove("VOLTAGE_STREAM_TOKEN")
        .args(["checkout", "events", "watch", "-n"])
        .assert()
        .success();
    let data = json_stdout(&watched)["data"].clone();
    assert_eq!(data["run"], json!({"kind": "event_stream"}));
    assert_eq!(data["origin"], Value::Null);

    // The Origin header decides whether a browser-bound session accepts the request.
    let session = cli(dir.path(), &server)
        .env_remove("VOLTAGE_CHECKOUT_TOKEN")
        .args([
            "checkout",
            "sessions",
            "get",
            RESOURCE,
            "--origin",
            "https://shop.example.test",
            "-n",
        ])
        .assert()
        .success();
    assert_eq!(
        json_stdout(&session)["data"]["origin"],
        "https://shop.example.test"
    );
    assert!(server.received_requests().await.unwrap().is_empty());
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

#[tokio::test(flavor = "multi_thread")]
async fn human_output_fits_the_terminal_and_scrubs_api_text() {
    let (server, dir) = fixture().await;
    let hostile = "evil\u{1b}]52;c;AAAA\u{7}\u{202e}name";
    respond_once(
        &server,
        route("GET", payments_path()),
        ok(json!({"items": [{
            "id": RESOURCE, "status": "completed", "description": hostile,
            "amount": {"amount": 150000, "currency": "btc"},
            "created_at": "2026-09-29T10:00:00Z"
        }], "has_more": false})),
    )
    .await;
    let listed = Command::from_std(human_process_at(dir.path(), &server.uri()))
        .env("COLUMNS", "64")
        .args([
            "payments", "list", "--org", ORG, "--env", ENV, "--output", "table",
        ])
        .assert()
        .success();
    assert_eq!(
        stdout(&listed),
        format!(
            "{:<36}  {:<9}  AMOUNT\n{RESOURCE}  completed  150000 msats\n\
             Hidden columns: created_at. Use a wider terminal or --json for every field.\n\
             1 result.\n",
            "ID", "STATUS"
        )
    );

    route("GET", payment_path())
        .respond_with(in_order(vec![
            ok(json!({"id": RESOURCE, "status": hostile})),
            ok(json!({"id": RESOURCE, "status": "completed", "description": hostile})),
        ]))
        .expect(2)
        .mount(&server)
        .await;
    let waited = Command::from_std(human_process_at(dir.path(), &server.uri()))
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
            "--output",
            "table",
        ])
        .assert()
        .success();
    for text in [stdout(&waited), stderr(&waited)] {
        assert!(
            !text
                .chars()
                .any(|c| (c.is_control() && c != '\n') || c == '\u{202e}'),
            "{text:?}"
        );
    }
    assert!(
        stderr(&waited).contains("status: evil ]52;c;AAAA  name; waiting for completed"),
        "{}",
        stderr(&waited)
    );
    assert!(stdout(&waited).contains("description  evil ]52;c;AAAA  name\n"));
}

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
            "webhooks", "keys", "rotate", RESOURCE, "--org", ORG, "--env", ENV,
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
// Interactive scope pickers
// ---------------------------------------------------------------------------------------

/// What a command did on a pseudo-terminal.
#[cfg(unix)]
struct TerminalRun {
    code: Option<i32>,
    /// The terminal's local modes before the command started and after it exited.
    modes: (libc::tcflag_t, libc::tcflag_t),
    shown: String,
    stdout: String,
}

/// Run a command with stdin and stderr on a pseudo-terminal and stdout piped. Once the
/// terminal shows `prompt`, type `keys`; without a prompt, nothing is typed.
#[cfg(unix)]
async fn on_a_terminal(
    dir: &Path,
    server: &MockServer,
    args: &[&str],
    prompt: Option<&'static str>,
    keys: &'static [u8],
) -> TerminalRun {
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let (mut controller, terminal) = open_pty();
    let observer = controller.try_clone().unwrap();
    let before = local_modes(&observer);
    let mut command = human_process_at(dir, &server.uri());
    command
        .env_remove("VOLTAGE_EXECUTE")
        .args(args)
        .stdin(Stdio::from(terminal.try_clone().unwrap()))
        .stderr(Stdio::from(terminal))
        .stdout(Stdio::piped());
    // SAFETY: as in `interrupt_the_hidden_key_prompt`: setsid and ioctl are
    // async-signal-safe, and the closure allocates nothing.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().unwrap();
    let (prompted, shown_prompt) = tokio::sync::oneshot::channel();
    let drain = tokio::task::spawn_blocking(move || {
        let mut prompted = Some(prompted);
        let mut shown = Vec::new();
        let mut chunk = [0; 4096];
        while let Ok(read @ 1..) = controller.read(&mut chunk) {
            shown.extend_from_slice(&chunk[..read]);
            if prompt.is_some_and(|prompt| String::from_utf8_lossy(&shown).contains(prompt)) {
                prompted.take().map(|prompted| prompted.send(()));
            }
        }
        String::from_utf8_lossy(&shown).into_owned()
    });
    if prompt.is_some() {
        tokio::time::timeout(Duration::from_secs(10), shown_prompt)
            .await
            .unwrap()
            .unwrap();
        (&observer).write_all(keys).unwrap();
    }
    let pid = child.id().to_string();
    let Ok(output) = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    else {
        std::process::Command::new("kill")
            .args(["-KILL", &pid])
            .status()
            .unwrap();
        panic!("the command on the terminal did not exit");
    };
    let output = output.unwrap();
    let after = local_modes(&observer);
    drop(command);
    let shown = tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .unwrap()
        .unwrap();
    TerminalRun {
        code: output.status.code(),
        modes: (before, after),
        shown,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_picks_a_missing_wallet_and_scripts_are_never_asked() {
    let (server, dir) = fixture().await;
    Mock::given(method("GET"))
        .and(path(wallets_path(ORG)))
        .and(query_param("environment_id", ENV))
        .respond_with(ok(json!([
            {"id": RESOURCE, "name": "ops"},
            {"id": WALLET, "name": "Payroll"}
        ])))
        .mount(&server)
        .await;
    let send = [
        "payments",
        "send",
        "--org",
        ORG,
        "--env",
        ENV,
        "--currency",
        "btc",
        "--invoice",
        "lnbc1",
        "--output",
        "table",
    ];
    let listings = || async {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == wallets_path(ORG))
            .count()
    };

    // Typing filters the list, and Enter picks; the change is still only described.
    let picked = on_a_terminal(dir.path(), &server, &send, Some("Wallet:"), b"Pay\r").await;
    assert_eq!(picked.code, Some(6), "{}", picked.shown);
    assert!(
        picked.stdout.contains("body.wallet_id"),
        "{}",
        picked.stdout
    );
    assert!(picked.stdout.contains(WALLET), "{}", picked.stdout);
    assert!(
        picked
            .shown
            .contains(&format!("Next time, pass --wallet {WALLET}"))
    );
    assert_eq!(picked.modes.0, picked.modes.1);

    // Esc leaves the scope unset, so the command fails as it did before pickers.
    let escaped = on_a_terminal(dir.path(), &server, &send, Some("Wallet:"), b"\x1b").await;
    assert_eq!(escaped.code, Some(2), "{}", escaped.shown);
    assert!(
        escaped
            .shown
            .contains("--wallet is required for payment creation")
    );
    assert_eq!(escaped.modes.0, escaped.modes.1);

    // A typed Ctrl-C reaches the picker as a key in raw mode and still exits with 130.
    let interrupted = on_a_terminal(dir.path(), &server, &send, Some("Wallet:"), b"\x03").await;
    assert_eq!(interrupted.code, Some(130), "{}", interrupted.shown);
    assert!(
        interrupted
            .shown
            .contains("Interrupted before any resource change was submitted")
    );
    assert_eq!(interrupted.modes.0, interrupted.modes.1);
    assert_eq!(listings().await, 3);

    // JSON output, --no-input, and dry runs neither ask nor list.
    for extra in ["--json", "--no-input", "--dry-run"] {
        let mut args = send.to_vec();
        args.push(extra);
        let run = on_a_terminal(dir.path(), &server, &args, None, b"").await;
        assert_eq!(run.code, Some(2), "{extra}: {}", run.shown);
        assert!(
            run.shown
                .contains("--wallet is required for payment creation"),
            "{extra}"
        );
        assert!(!run.shown.contains("Wallet:"), "{extra}");
    }
    assert_eq!(listings().await, 3);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_api_key_without_an_organization_keeps_the_original_error() {
    let (server, dir) = fixture().await;
    let run = on_a_terminal(
        dir.path(),
        &server,
        &["wallets", "list", "--output", "table"],
        None,
        b"",
    )
    .await;
    assert_eq!(run.code, Some(2), "{}", run.shown);
    assert!(
        run.shown.contains("--org is required for wallets list"),
        "{}",
        run.shown
    );
    assert!(run.shown.contains("--profile NAME"), "{}", run.shown);
    assert!(server.received_requests().await.unwrap().is_empty());
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
