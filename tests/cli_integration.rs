//! Process-level behavior of the `voltage` binary against mock HTTP services.
//!
//! Every test runs the built binary in a private temporary configuration directory with a
//! fixed API key, talks to a wiremock server started on port 0, and never reaches a real
//! service. Payment waits poll with jittered backoff, so tests count requests only where the
//! mocked response sequence makes the count deterministic.

use assert_cmd::{Command, assert::Assert};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    ffi::OsString,
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
    // The hint names the command that finds the missing value.
    assert!(error.contains("hint: Find your organization ID with `voltage organizations list`"));
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
    let running = KillOnDrop::new(&child);
    drop(child.stdout.take());
    let output = exit_within(
        child,
        running,
        Duration::from_secs(10),
        "the command did not exit after its stdout closed",
    )
    .await;
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
    // A saved key never uses the auth service, so its URL is the default, not the profile's.
    assert_eq!(
        data["auth_url"],
        setting(json!(config::AUTH_URL), "default")
    );
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
async fn context_reports_the_auth_url_commands_would_use() {
    let (server, dir) = fixture().await;
    let auth = MockServer::start().await;
    seeded_login(dir.path(), &auth, false);
    let setting = |value: &str, source: &str| json!({"value": value, "source": source});
    let context = |args: &[&str], ambient: bool| {
        let mut command = cli(dir.path(), &server);
        if !ambient {
            command.env_remove("VOLTAGE_API_KEY");
        }
        json_stdout(&command.arg("context").args(args).assert().success())["data"].clone()
    };

    // The ambient key uses no auth service: the built-in default, or the normalized flag.
    let data = context(&[], true);
    assert_eq!(data["auth_url"], setting(config::AUTH_URL, "default"));
    let data = context(&["--auth-url", "https://auth.example.com/api/v1/"], true);
    assert_eq!(
        data["auth_url"],
        setting("https://auth.example.com/api/v1", "flag")
    );
    assert_eq!(data["credential_problem"], Value::Null);

    // A browser login reports the URL it was saved with, sourced like the credential. A flag
    // that normalizes to it is no problem.
    let saved = auth.uri();
    let data = context(&[], false);
    assert_eq!(data["auth_url"], setting(&saved, "default"));
    let data = context(
        &["--account", LOGIN_NAME, "--auth-url", &format!("{saved}/")],
        false,
    );
    assert_eq!(data["auth_url"], setting(&saved, "flag"));
    assert_eq!(data["credential_problem"], Value::Null);

    // A different flag is reported the way commands reject it.
    let other = ["--auth-url", "https://auth.example.com/api/v1"];
    let data = context(&other, false);
    assert_eq!(data["auth_url"], setting(&saved, "default"));
    assert_eq!(
        data["credential"],
        json!({"kind": "user", "location": "file"})
    );
    assert_eq!(data["credential_problem"], "auth_url_mismatch");
    let rejected = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(["wallets", "list", "--org", ORG, "--env", ENV])
        .args(other)
        .assert()
        .code(2);
    let shown = stderr(&rejected);
    assert!(
        shown.contains("Saved credentials cannot be refreshed at a different auth URL"),
        "{shown}"
    );
    assert!(shown.contains("Omit --auth-url"), "{shown}");
    // `login` refuses a saved name, so moving the login names a revoking logout first.
    assert!(
        shown.contains(
            "run `voltage logout --account NAME`, then `voltage login --account NAME --auth-url URL`"
        ),
        "{shown}"
    );

    // An invalid flag is an invalid URL, for context and commands alike.
    let invalid = ["--auth-url", "http://auth.example.com"];
    for command in [
        &["context"][..],
        &["wallets", "list", "--org", ORG, "--env", ENV],
    ] {
        let failed = cli(dir.path(), &server)
            .env_remove("VOLTAGE_API_KEY")
            .args(command)
            .args(invalid)
            .assert()
            .code(2);
        assert!(stderr(&failed).contains("Service URLs require HTTPS"));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(auth.received_requests().await.unwrap().is_empty());
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
    // A name in use is refused with the same message as `login`, and the saved key is kept.
    let in_use =
        "Credential staging-key already exists; log it out first or use a different --account name";
    let again = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(import)
        .write_stdin("again")
        .assert()
        .code(2);
    assert_eq!(json_error(&again)["error"]["message"], in_use);
    let login = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(["login", "--account", "staging-key", "--no-browser"])
        .assert()
        .code(2);
    assert_eq!(json_error(&login)["error"]["message"], in_use);
    let settings = Settings::open(dir.path().to_path_buf()).unwrap();
    let Credential::ApiKey(key) = settings.read_credential("staging-key").unwrap() else {
        panic!("expected an API key");
    };
    assert_eq!(key.expose(), "imported-secret");
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

/// A device login that is approved at once and signs in as `user_id`.
async fn approved_device_login(server: &MockServer, user_id: &str) {
    respond_once(
        server,
        route("POST", "/oauth/device_authorization"),
        ok(json!({
            "device_code":"device-secret","user_code":"ABCD-EFGH",
            "verification_uri":"https://app.voltage.cloud/cli/authorize","expires_in":60,"interval":5
        })),
    )
    .await;
    respond_once(
        server,
        route("POST", "/oauth/token"),
        ok(json!({
            "access_token":"new-access","refresh_token":"new-refresh",
            "token_type":"Bearer","expires_in":600
        })),
    )
    .await;
    respond_once(
        server,
        route("GET", "/users/current").and(header("authorization", "Bearer new-access")),
        ok(json!({"id":user_id,"email":LOGIN_EMAIL})),
    )
    .await;
}

fn login_as(dir: &Path, server: &MockServer, auth_url: &str) -> Command {
    let mut cmd = replacing_login(dir, server, auth_url);
    cmd.args(["--credential-store", "file"]);
    cmd
}

/// A login under the saved name without `--credential-store`, so it keeps the saved store.
fn replacing_login(dir: &Path, server: &MockServer, auth_url: &str) -> Command {
    let mut cmd = cli(dir, server);
    cmd.args([
        "login",
        "--auth-url",
        auth_url,
        "--no-browser",
        "--account",
        LOGIN_NAME,
    ]);
    cmd
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_of_the_same_user_replaces_the_saved_one_and_revokes_it() {
    for revoked in [true, false] {
        let (server, dir) = fixture().await;
        let mut settings = seeded_login(dir.path(), &server, true);
        approved_device_login(&server, RESOURCE).await;
        respond_once(
            &server,
            route("POST", "/oauth/revoke")
                .and(body_string_contains(format!("token={LOGIN_REFRESH}"))),
            if revoked {
                ResponseTemplate::new(200)
            } else {
                ResponseTemplate::new(503).set_body_json(json!({"error":"unavailable"}))
            },
        )
        .await;
        // Without `--credential-store`, the file-stored login stays in its file.
        let login = replacing_login(dir.path(), &server, &server.uri())
            .assert()
            .success();
        assert_eq!(
            json_stdout(&login)["data"],
            json!({"account":LOGIN_NAME,"email":LOGIN_EMAIL,"authenticated":true,"replaced":true})
        );
        settings.reload().unwrap();
        assert_eq!(
            settings.account(LOGIN_NAME).unwrap().store,
            CredentialStore::File
        );
        assert_eq!(
            login_tokens(&settings, LOGIN_NAME),
            ("new-access".to_owned(), "new-refresh".to_owned())
        );
        // A failed revocation keeps the new login and says how to end the old session.
        assert_eq!(
            stderr(&login).contains("Could not revoke the replaced CLI session"),
            !revoked,
            "{}",
            stderr(&login)
        );
        server.verify().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_of_another_user_keeps_the_saved_one_and_revokes_the_new_session() {
    let (server, dir) = fixture().await;
    let settings = seeded_login(dir.path(), &server, true);
    approved_device_login(&server, ORG).await;
    respond_once(
        &server,
        route("POST", "/oauth/revoke").and(body_string_contains("token=new-refresh")),
        ResponseTemplate::new(200),
    )
    .await;
    let login = login_as(dir.path(), &server, &server.uri())
        .assert()
        .code(2);
    assert_eq!(
        json_error(&login)["error"]["message"],
        format!(
            "Credential {LOGIN_NAME} already exists; log it out first or use a different --account name"
        )
    );
    assert_eq!(
        login_tokens(&settings, LOGIN_NAME),
        (LOGIN_ACCESS.to_owned(), LOGIN_REFRESH.to_owned())
    );
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_at_another_auth_url_is_refused_before_the_browser_step() {
    let (server, dir) = fixture().await;
    let settings = seeded_login(dir.path(), &server, true);
    let other = MockServer::start().await;
    let login = login_as(dir.path(), &server, &other.uri()).assert().code(2);
    assert_eq!(
        json_error(&login)["error"]["message"],
        format!(
            "Credential {LOGIN_NAME} already exists; log it out first or use a different --account name"
        )
    );
    assert!(other.received_requests().await.unwrap().is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(login_tokens(&settings, LOGIN_NAME).1, LOGIN_REFRESH);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saved_login_without_a_user_id_is_not_replaced() {
    let (server, dir) = fixture().await;
    let settings = seeded_login(dir.path(), &server, true);
    settings
        .write_credential(
            LOGIN_NAME,
            CredentialStore::File,
            &Credential::Login(Login {
                access_token: Secret::new(LOGIN_ACCESS.into()),
                refresh_token: Secret::new(LOGIN_REFRESH.into()),
                expires_at: 0,
                user_id: None,
                email: Some(LOGIN_EMAIL.into()),
            }),
        )
        .unwrap();
    approved_device_login(&server, RESOURCE).await;
    respond_once(
        &server,
        route("POST", "/oauth/revoke").and(body_string_contains("token=new-refresh")),
        ResponseTemplate::new(200),
    )
    .await;
    let login = login_as(dir.path(), &server, &server.uri())
        .assert()
        .code(2);
    assert_eq!(
        json_error(&login)["error"]["message"],
        format!(
            "Credential {LOGIN_NAME} already exists; log it out first or use a different --account name"
        )
    );
    assert_eq!(login_tokens(&settings, LOGIN_NAME).1, LOGIN_REFRESH);
    server.verify().await;
}

/// A saved login whose secret is corrupt or gone is refused before the browser step, with
/// the command that removes it, and that command works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saved_login_that_cannot_be_read_is_refused_before_the_browser_step() {
    for corrupt in [true, false] {
        let (server, dir) = fixture().await;
        seeded_login(dir.path(), &server, true);
        let mut files = std::fs::read_dir(dir.path().join("credentials")).unwrap();
        let file = files.next().unwrap().unwrap().path();
        assert!(files.next().is_none());
        if corrupt {
            std::fs::write(&file, "not json").unwrap();
        } else {
            std::fs::remove_file(&file).unwrap();
        }
        let login = login_as(dir.path(), &server, &server.uri())
            .assert()
            .code(2);
        let report = json_error(&login);
        assert_eq!(
            report["error"]["message"],
            format!(
                "Credential {LOGIN_NAME} already exists; log it out first or use a different --account name"
            )
        );
        assert_eq!(
            report["error"]["hint"],
            format!(
                "Its saved secret cannot be read. Remove it with `voltage logout --local --account {LOGIN_NAME}`."
            )
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        cli(dir.path(), &server)
            .args(["logout", "--account", LOGIN_NAME, "--local"])
            .assert()
            .success();
        assert!(!file.exists());
        let mut settings = settings_at(dir.path());
        settings.reload().unwrap();
        assert!(settings.config.accounts.is_empty());
    }
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

/// Kills a CLI process when dropped, until `exit_within` reaps it. A test that fails while
/// the process still runs, for example at a prompt that never appears, would otherwise leave
/// it running, and with it each blocking task that waits for it or reads its terminal. The
/// test runtime waits for those tasks when it shuts down, so one failure would hang the run.
#[cfg(unix)]
struct KillOnDrop(Option<u32>);

#[cfg(unix)]
impl KillOnDrop {
    fn new(child: &std::process::Child) -> Self {
        Self(Some(child.id()))
    }
}

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // Until the process is reaped, its ID stays its own, so a kill after it exited does
        // nothing.
        if let Some(pid) = self.0 {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
    }
}

/// Wait up to `limit` for `child` to exit. If it does not, fail with `what`, and `running`
/// kills it as the test unwinds. A reaped child's ID can be reused, so `running` is disarmed
/// once the child is reaped.
#[cfg(unix)]
async fn exit_within(
    child: std::process::Child,
    mut running: KillOnDrop,
    limit: Duration,
    what: &str,
) -> std::process::Output {
    let waited = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap());
    let Ok(output) = tokio::time::timeout(limit, waited).await else {
        panic!("{what}");
    };
    running.0 = None;
    output.unwrap()
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
    let running = KillOnDrop::new(&child);
    // Generous: a device login waits out its first poll interval before any request.
    tokio::time::timeout(Duration::from_secs(15), arrived.notified())
        .await
        .unwrap();
    interrupt(&child);
    let output = exit_within(
        child,
        running,
        Duration::from_secs(5),
        "Ctrl-C during a request did not end the process",
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// The JSON error report: the last line of stderr, after any recovery notice.
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
    let running = KillOnDrop::new(&child);
    let reader = tokio::task::spawn_blocking(move || {
        let mut shown = Vec::new();
        // Linux PTYs return EIO rather than EOF when the terminal end closes.
        let _ = controller.read_to_end(&mut shown);
        shown
    });
    let output = exit_within(
        child,
        running,
        Duration::from_secs(10),
        "the listing did not exit",
    )
    .await;
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
    let running = KillOnDrop::new(&child);
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
    let code = exit_within(
        child,
        running,
        Duration::from_secs(5),
        "Ctrl-C at the hidden key prompt did not end the process",
    )
    .await
    .status
    .code();
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

/// Once the new login is saved, an interruption while the replaced session is revoked says
/// the new login is saved, not that a session may have gone unsaved.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupt_while_revoking_a_replaced_login_says_the_new_one_is_saved() {
    let (server, dir) = fixture().await;
    let mut settings = seeded_login(dir.path(), &server, true);
    approved_device_login(&server, RESOURCE).await;
    let (responder, arrived) = announced(ok(json!({})).set_delay(Duration::from_secs(10)));
    respond_once(
        &server,
        route("POST", "/oauth/revoke").and(body_string_contains(format!("token={LOGIN_REFRESH}"))),
        responder,
    )
    .await;
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
        ],
    )
    .await;
    let report = error_report(&output.stderr);
    assert_eq!(
        report["error"]["message"],
        format!(
            "Interrupted while revoking the replaced session for {LOGIN_NAME}; the new login is saved"
        )
    );
    settings.reload().unwrap();
    assert_eq!(login_tokens(&settings, LOGIN_NAME).1, "new-refresh");
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
    let running = KillOnDrop::new(&child);
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
    let output = exit_within(
        child,
        running,
        Duration::from_secs(5),
        "Ctrl-C during a blocked --data read did not end the process",
    )
    .await;
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
async fn a_described_change_sends_nothing_records_nothing_and_omits_private_values() {
    let (server, dir) = fixture().await;
    let output_file = dir.path().join("result.json");
    let planned = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
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
        ])
        .arg("--output-file")
        .arg(&output_file)
        .assert()
        .code(6);
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
        .env_remove("VOLTAGE_EXECUTE")
        .args(["wallets", "delete", WALLET, "--org", ORG, "--env", ENV])
        .assert()
        .code(6);
    let data = json_stdout(&delete)["data"].clone();
    assert_eq!(data["method"], "DELETE");
    assert_eq!(data["run"]["verify_wallet_environment"], true);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn webhook_test_without_data_sends_the_test_event() {
    let (server, dir) = fixture().await;
    let test_path = format!("/organizations/{ORG}/environments/{ENV}/webhooks/{RESOURCE}/test");
    respond_once(&server, route("POST", test_path), accepted()).await;
    let sent = cli(dir.path(), &server)
        .args([
            "webhooks",
            "test",
            RESOURCE,
            "--org",
            ORG,
            "--env",
            ENV,
            "--execute",
        ])
        .assert()
        .success();
    assert_eq!(json_stdout(&sent)["http_status"], 202);
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["payload"]["type"], "test");
    assert_eq!(body["payload"]["detail"]["event"], "created");
    assert!(
        body["delivery_id"]
            .as_str()
            .unwrap()
            .parse::<uuid::Uuid>()
            .is_ok()
    );
}

#[test]
fn requests_without_a_response_say_why_and_name_the_next_step() {
    let dir = private_tempdir();
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let closed = format!("http://{closed}");
    let api_hint =
        "Check the API URL, your network or proxy, and whether the API service is running.";
    let cases: [(Vec<&str>, &str, &str); 5] = [
        (
            vec!["wallets", "list", "--org", ORG],
            "HTTP read request failed: could not connect",
            api_hint,
        ),
        (
            vec!["wallets", "delete", WALLET, "--org", ORG],
            "HTTP request failed: could not connect; a write may have been submitted. No mutation was retried.",
            api_hint,
        ),
        (
            vec![
                "checkout",
                "events",
                "watch",
                "--origin",
                "https://shop.example.test",
            ],
            "Opening the checkout event stream failed: could not connect",
            api_hint,
        ),
        (
            vec![
                "convert",
                "10",
                "usd",
                "--to",
                "btc",
                "--price-url",
                &closed,
            ],
            "Price request failed: could not connect",
            "Check the price URL, your network or proxy, and whether the price service is running.",
        ),
        (
            vec!["login", "--no-browser", "--auth-url", &closed],
            "Authentication request failed: could not connect; no token request was automatically retried",
            "Check the auth URL, your network or proxy, and whether the auth service is running.",
        ),
    ];
    for (args, message, hint) in cases {
        let failed = cli_at(dir.path(), &closed)
            .env("VOLTAGE_STREAM_TOKEN", "private-stream-token")
            .args(&args)
            .assert()
            .code(4);
        let report = json_error(&failed);
        assert_eq!(report["error"]["message"], message, "{args:?}");
        assert_eq!(report["error"]["hint"], hint, "{args:?}");
        if args[1] == "delete" {
            // A write keeps its unknown-outcome detail.
            assert_eq!(report["error"]["detail"]["outcome"], "unknown");
        }
    }

    // A server that takes the request and never answers: a write may have been applied, so
    // its hint says to look the resource up first, never to retry.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let silent_url = format!("http://{}", silent.local_addr().unwrap());
    let writes: [Vec<&str>; 2] = [
        vec!["wallets", "delete", WALLET, "--org", ORG],
        vec![
            "payments",
            "send",
            "--org",
            ORG,
            "--env",
            ENV,
            "--wallet",
            WALLET,
            "--currency",
            "btc",
            "--address",
            "tb1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3q0sl5k7",
            "--amount",
            "1000",
            "--unit",
            "sats",
        ],
    ];
    for args in writes {
        let failed = cli_at(dir.path(), &silent_url)
            .args(["--timeout", "1"])
            .args(&args)
            .assert()
            .code(4);
        let report = json_error(&failed);
        let hint = report["error"]["hint"].as_str().unwrap();
        assert!(
            hint.starts_with("The change may have been applied. Look up the resource ID before you run the command again"),
            "{args:?}: {hint}"
        );
        assert!(!hint.contains("Retry"), "{args:?}: {hint}");
        assert!(
            report["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no response within --timeout"),
            "{args:?}"
        );
        assert_eq!(report["error"]["detail"]["outcome"], "unknown", "{args:?}");
    }
    // A read that times out may simply be retried.
    let read = cli_at(dir.path(), &silent_url)
        .args(["--timeout", "1", "wallets", "list", "--org", ORG])
        .assert()
        .code(4);
    assert_eq!(
        json_error(&read)["error"]["hint"],
        "Retry, or pass a larger --timeout."
    );
    drop(silent);
}

#[tokio::test]
async fn output_file_refusals_name_the_path_problem_and_send_nothing() {
    let (server, dir) = fixture().await;
    let existing = dir.path().join("result.json");
    std::fs::write(&existing, "kept").unwrap();
    let delete = ["wallets", "delete", WALLET, "--org", ORG, "--execute"];
    let refused = cli(dir.path(), &server)
        .args(delete)
        .arg("--output-file")
        .arg(&existing)
        .assert()
        .code(2);
    let error = &json_error(&refused)["error"];
    assert_eq!(
        error["message"],
        format!(
            "{} already exists; --output-file writes only to a new file",
            existing.display()
        )
    );
    assert_eq!(
        error["hint"],
        "Pass a path that does not exist yet, or move the existing file. No change was sent."
    );
    assert_eq!(std::fs::read_to_string(&existing).unwrap(), "kept");

    // Any other failure keeps its cause and gets the same next step.
    let missing_dir = dir.path().join("missing").join("result.json");
    let refused = cli(dir.path(), &server)
        .args(delete)
        .arg("--output-file")
        .arg(&missing_dir)
        .assert()
        .code(2);
    let error = &json_error(&refused)["error"];
    assert!(
        error["message"].as_str().unwrap().starts_with(&format!(
            "Cannot reserve output {}: ",
            missing_dir.display()
        )),
        "{error}"
    );
    assert_eq!(
        error["hint"],
        "Pass a path in a directory you can write to. No change was sent."
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
    // VOLTAGE_EXECUTE=0 describes one command even when a script exported it as 1.
    cli(dir.path(), &server)
        .env("VOLTAGE_EXECUTE", "0")
        .args(delete)
        .assert()
        .code(6);
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
async fn a_described_change_reads_raw_bodies_and_shows_them_only_with_show_secrets() {
    let (server, dir) = fixture().await;
    let body = json!({"id":RESOURCE,"wallet_id":WALLET,"currency":"btc","type":"bolt11","data":{"payment_request":"lnbc1raw"},"extension":{"note":"x"}});
    let args = [
        "payments", "create", "--org", ORG, "--env", ENV, "--data", "-",
    ];
    let hidden = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(args)
        .write_stdin(body.to_string())
        .assert()
        .code(6);
    let data = json_stdout(&hidden)["data"].clone();
    assert_eq!(data["body"]["data"]["payment_request"], "[OMITTED]");
    assert_eq!(data["body"]["extension"], "[OMITTED]");
    assert!(data["notes"].as_array().unwrap().contains(&json!(
        "The --data payload was read and validated; a real run reads it again."
    )));
    let shown = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(args)
        .arg("--show-secrets")
        .write_stdin(body.to_string())
        .assert()
        .code(6);
    let data = json_stdout(&shown)["data"].clone();
    assert_eq!(data["body"], body);
    assert_eq!(data["values_omitted"], false);
    // Local validation still applies.
    cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(args)
        .write_stdin(json!({"wallet_id": WALLET}).to_string())
        .assert()
        .code(2);
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(!dir.path().join("requests").exists());
}

#[tokio::test]
async fn a_payment_without_currency_takes_its_wallets_line_of_credit_currency() {
    let (server, dir) = fixture().await;
    const LINE: &str = "55555555-5555-4555-8555-555555555555";
    respond_once(
        &server,
        route("GET", format!("/organizations/{ORG}/wallets/{WALLET}")),
        ok(json!({"id": WALLET, "line_of_credit_id": LINE})),
    )
    .await;
    respond_once(
        &server,
        route(
            "GET",
            format!("/organizations/{ORG}/lines_of_credit/{LINE}/summary"),
        ),
        ok(json!({"id": LINE, "currency": "btc"})),
    )
    .await;
    let send = [
        "payments",
        "send",
        "--org",
        ORG,
        "--env",
        ENV,
        "--wallet",
        WALLET,
        "--address",
        "bc1qexample",
        "--amount",
        "1500",
    ];
    // A description reads the wallet as a real run would, so both send the same body; the
    // amount without --unit is in the currency's base unit.
    let described = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(send)
        .assert()
        .code(6);
    let body = json_stdout(&described)["data"]["body"].clone();
    assert_eq!(body["currency"], "btc");
    assert_eq!(
        body["data"]["amount"],
        json!({"currency": "btc", "amount": 1500})
    );
    assert!(stderr(&described).contains("Using the wallet's currency, btc"));
    server.verify().await;
    let read = server.received_requests().await.unwrap().len();
    assert_eq!(read, 2);

    // An explicit --currency is kept, so nothing is read.
    let described = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args([
            "payments",
            "send",
            "--org",
            ORG,
            "--env",
            ENV,
            "--wallet",
            WALLET,
            "--currency",
            "btc",
            "--invoice",
            "lnbc1",
        ])
        .assert()
        .code(6);
    assert_eq!(json_stdout(&described)["data"]["body"]["currency"], "btc");
    assert!(!stderr(&described).contains("wallet's currency"));
    assert_eq!(server.received_requests().await.unwrap().len(), read);

    // Without a credential there is nothing to read the wallet with; the hint says both fixes.
    let missing = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .env_remove("VOLTAGE_API_KEY")
        .args(send)
        .assert()
        .code(3);
    assert!(stderr(&missing).contains("Or pass --currency to skip reading the wallet."));
    assert_eq!(server.received_requests().await.unwrap().len(), read);

    // Without a wallet there is nothing to read; the payment reports the missing wallet.
    let unscoped: Vec<&str> = send
        .iter()
        .copied()
        .filter(|arg| !matches!(*arg, "--wallet" | WALLET))
        .collect();
    let error = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(&unscoped)
        .assert()
        .code(2);
    assert!(stderr(&error).contains("--wallet is required"));
    assert_eq!(server.received_requests().await.unwrap().len(), read);

    // A wallet without a line of credit has no currency to take, so the hint names the flag.
    respond_once(
        &server,
        route("GET", format!("/organizations/{ORG}/wallets/{RESOURCE}")),
        ok(json!({"id": RESOURCE})),
    )
    .await;
    let mut other = send;
    other[7] = RESOURCE;
    let error = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(other)
        .assert()
        .code(2);
    assert!(stderr(&error).contains("Cannot tell this wallet's currency"));
    assert!(stderr(&error).contains("Pass --currency btc or --currency usd."));
}

/// A described send without `--currency` from `wallet`, which reads that wallet.
fn currency_from_wallet(wallet: &str) -> Vec<&str> {
    vec![
        "payments",
        "send",
        "--org",
        ORG,
        "--env",
        ENV,
        "--wallet",
        wallet,
        "--invoice",
        "lnbc1",
        "--amount",
        "250",
    ]
}

/// Serve `wallet` with the line of credit `line`, and that line's summary as `summary`.
async fn serve_line_of_credit(
    server: &MockServer,
    wallet: &str,
    line: &str,
    summary: ResponseTemplate,
    times: u64,
) {
    let wallet_read = ok(json!({"id": wallet, "line_of_credit_id": line}));
    let wallet_path = format!("/organizations/{ORG}/wallets/{wallet}");
    respond(server, route("GET", wallet_path), wallet_read, times).await;
    let summary_path = format!("/organizations/{ORG}/lines_of_credit/{line}/summary");
    respond(server, route("GET", summary_path), summary, times).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bolt11_send_without_a_wallet_shortlists_the_wallets_that_could_pay() {
    let (server, dir) = fixture().await;
    const USD: &str = "55555555-5555-4555-8555-555555555555";
    const MAINNET: &str = "66666666-6666-4666-8666-666666666666";
    const SHORT: &str = "77777777-7777-4777-8777-777777777777";
    let funded = |currency: &str| json!([{"currency": currency, "available": {"amount": 5_000, "negative": false}}]);
    Mock::given(method("GET"))
        .and(path(wallets_path(ORG)))
        .and(query_param("environment_id", ENV))
        .respond_with(ok(json!([
            {"id": WALLET, "name": "ops", "active": true, "network": "mutinynet",
             "balances": funded("btc")},
            {"id": USD, "name": "payroll", "active": true, "network": "signet",
             "balances": funded("usd")},
            {"id": MAINNET, "name": "treasury", "active": true, "network": "mainnet",
             "balances": funded("btc")},
            {"id": RESOURCE, "name": "empty", "active": true, "network": "mutinynet",
             "balances": [{"currency": "btc", "available": {"amount": 9, "negative": true}}]},
            {"id": SHORT, "name": "short", "active": true, "network": "signet", "limit": 500,
             "balances": [{"currency": "btc", "available": {"amount": 400, "negative": false}}]}
        ])))
        .mount(&server)
        .await;
    let send = |extra: &[&str]| {
        let mut args = vec![
            "payments",
            "send",
            "--org",
            ORG,
            "--env",
            ENV,
            "--invoice",
            "lntbs10n1qqq",
        ];
        args.extend_from_slice(extra);
        cli(dir.path(), &server)
            .env_remove("VOLTAGE_EXECUTE")
            .args(args)
            .assert()
    };
    let listings = || async {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == wallets_path(ORG))
            .count()
    };

    // Without --currency, a BTC and a USD wallet on the invoice's signet both qualify; the
    // others say why not. A description lists the wallets as a real run would.
    let error = json_error(&send(&[]).code(2));
    assert_eq!(
        error["error"]["message"],
        "--wallet is required for payment creation"
    );
    let detail = &error["error"]["detail"];
    assert_eq!(
        detail["invoice"],
        json!({"network": "signet", "amount_msats": 1000})
    );
    let flags = |id: &str| format!("--wallet {id} --org {ORG} --env {ENV}");
    assert_eq!(
        detail["candidates"],
        json!([
            {"wallet_id": WALLET, "name": "ops", "network": "mutinynet", "flags": flags(WALLET),
             "needs_quote": false},
            {"wallet_id": USD, "name": "payroll", "network": "signet", "flags": flags(USD),
             "needs_quote": true,
             "quote": format!("voltage quotes create --credit-line CREDIT_LINE_ID --network signet --amount 1 --unit sats --to usd --org {ORG} --env {ENV} --execute")}
        ])
    );
    assert_eq!(
        detail["excluded"],
        json!([
            {"wallet_id": MAINNET, "name": "treasury", "network": "mainnet", "reason": "wrong_network"},
            {"wallet_id": RESOURCE, "name": "empty", "network": "mutinynet", "reason": "no_balance"},
            {"wallet_id": SHORT, "name": "short", "network": "signet", "reason": "insufficient_balance"}
        ])
    );
    assert!(
        detail["basis"]
            .as_str()
            .unwrap()
            .contains("a shortlist, not a guarantee")
    );
    let hint = error["error"]["hint"].as_str().unwrap();
    assert!(
        hint.contains(&format!("`{}` (ops)", flags(WALLET))),
        "{hint}"
    );
    assert_eq!(listings().await, 1);

    // --currency usd leaves only the USD wallet.
    let error = json_error(&send(&["--currency", "usd"]).code(2));
    let candidates = error["error"]["detail"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0]["wallet_id"], USD);
    assert_eq!(listings().await, 2);

    // When none qualifies, human output names each wallet's reason in the hint.
    let human = Command::from_std(human_process_at(dir.path(), &server.uri()))
        .env_remove("VOLTAGE_EXECUTE")
        .args([
            "payments",
            "send",
            "--org",
            ORG,
            "--env",
            ENV,
            "--currency",
            "usd",
            "--invoice",
            "lnbc10u1qqq",
            "--output",
            "table",
        ])
        .assert()
        .code(2);
    let shown = stderr(&human);
    assert!(
        shown.contains("No wallet in this scope is active on mainnet with usd funds"),
        "{shown}"
    );
    assert!(
        shown.contains(&format!("treasury ({MAINNET}) holds no usd")),
        "{shown}"
    );
    assert!(
        shown.contains(&format!("ops ({WALLET}) is on mutinynet")),
        "{shown}"
    );
    // The hint already gives the shortlist, so human output adds no details block.
    assert!(!shown.contains("details:"), "{shown}");
    assert_eq!(listings().await, 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bolt11_send_without_an_environment_names_each_candidates_environment() {
    let (server, dir) = fixture().await;
    Mock::given(method("GET"))
        .and(path(wallets_path(ORG)))
        .respond_with(ok(json!([
            {"id": WALLET, "name": "ops", "active": true, "network": "mutinynet",
             "environment_id": ENV,
             "balances": [{"currency": "btc", "available": {"amount": 5_000, "negative": false}}]}
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let output = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args([
            "payments",
            "send",
            "--org",
            ORG,
            "--invoice",
            "lntbs10n1qqq",
        ])
        .assert()
        .code(2);
    let error = json_error(&output);
    let flags = format!("--wallet {WALLET} --org {ORG} --env {ENV}");
    assert_eq!(error["error"]["detail"]["candidates"][0]["flags"], flags);
    // The listing spanned the organization: no environment was sent.
    let listed = server.received_requests().await.unwrap();
    let listing = listed
        .iter()
        .find(|request| request.url.path() == wallets_path(ORG))
        .unwrap();
    assert!(
        !listing
            .url
            .query_pairs()
            .any(|(key, _)| key == "environment_id"),
        "{}",
        listing.url
    );
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bolt11_send_without_a_wallet_fails_before_listing_when_it_cannot_shortlist() {
    let (server, dir) = fixture().await;
    let send = |args: &[&str]| {
        let mut full = vec!["payments", "send", "--env", ENV];
        full.extend_from_slice(args);
        cli(dir.path(), &server)
            .env_remove("VOLTAGE_EXECUTE")
            .args(full)
            .assert()
    };

    // An invoice without a BOLT11 prefix cannot be matched, so nothing is listed.
    let error = json_error(&send(&["--org", ORG, "--invoice", "bitcoin:bc1q"]).code(2));
    assert_eq!(
        error["error"]["message"],
        "--wallet is required for payment creation"
    );
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .starts_with("The invoice is not a BOLT11 invoice the CLI can read")
    );
    // So is one with a BOLT11 prefix but an amount it cannot read.
    let error = json_error(&send(&["--org", ORG, "--invoice", "lnbc1p1qqq"]).code(2));
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .starts_with("The invoice is not a BOLT11 invoice the CLI can read")
    );
    // A local mistake is reported first.
    let error = json_error(&send(&["--org", ORG, "--invoice", "lnbc1", "--unit", "sats"]).code(2));
    assert_eq!(error["error"]["message"], "--unit requires --amount");
    // Without an organization there is nothing to list; the payment reports what is missing.
    let error = json_error(&send(&["--invoice", "lnbc1"]).code(2));
    assert!(error["error"]["detail"].is_null(), "{error}");
    // An on-chain send is not shortlisted.
    let error = json_error(&send(&["--org", ORG, "--address", "bc1q"]).code(2));
    assert_eq!(
        error["error"]["message"],
        "--wallet is required for payment creation"
    );
    assert!(error["error"]["detail"].is_null());
    assert!(server.received_requests().await.unwrap().is_empty());

    // Without a credential, or when the listing fails, the error says how to skip listing.
    let missing = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .env_remove("VOLTAGE_API_KEY")
        .args([
            "payments",
            "send",
            "--org",
            ORG,
            "--env",
            ENV,
            "--invoice",
            "lnbc1",
        ])
        .assert()
        .code(3);
    assert!(stderr(&missing).contains("Or pass --wallet ID to skip listing wallets."));
    route("GET", wallets_path(ORG))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let failed = send(&["--org", ORG, "--invoice", "lnbc1"]).failure();
    assert!(stderr(&failed).contains("Or pass --wallet ID to skip listing wallets."));
}

#[tokio::test]
async fn a_wallet_whose_currency_cannot_be_read_fails_and_names_the_flag() {
    let (server, dir) = fixture().await;
    let unknown = "55555555-5555-4555-8555-555555555551";
    let missing = "55555555-5555-4555-8555-555555555552";
    let forbidden = "55555555-5555-4555-8555-555555555553";
    let absent = "55555555-5555-4555-8555-555555555554";
    let asset = "55555555-5555-4555-8555-555555555555";
    serve_line_of_credit(
        &server,
        unknown,
        RESOURCE,
        ok(json!({"currency": "eur"})),
        1,
    )
    .await;
    serve_line_of_credit(&server, missing, ENV, ok(json!({"id": ENV})), 1).await;
    let denied = ResponseTemplate::new(403).set_body_json(json!({"error": "forbidden"}));
    serve_line_of_credit(&server, forbidden, ORG, denied, 1).await;
    respond_once(
        &server,
        route("GET", format!("/organizations/{ORG}/wallets/{absent}")),
        ResponseTemplate::new(404).set_body_json(json!({"error": "not_found"})),
    )
    .await;
    // A summary with an unknown or no currency is not guessed at.
    for wallet in [unknown, missing] {
        let error = cli(dir.path(), &server)
            .env_remove("VOLTAGE_EXECUTE")
            .args(currency_from_wallet(wallet))
            .assert()
            .code(2);
        assert!(stderr(&error).contains("Cannot tell this wallet's currency"));
        assert!(stderr(&error).contains("Pass --currency btc or --currency usd."));
    }
    // An asset has no --currency to name it, so the hint points to a complete body.
    serve_line_of_credit(
        &server,
        asset,
        "66666666-6666-4666-8666-666666666666",
        ok(json!({"currency": "asset:ab12"})),
        1,
    )
    .await;
    let error = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(currency_from_wallet(asset))
        .assert()
        .code(2);
    assert!(stderr(&error).contains("This wallet holds an asset"));
    assert!(stderr(&error).contains("Send the complete payment body with --data @payment.json."));
    // A summary or wallet the API refuses reports the API's status, as any read does.
    for (wallet, code, status) in [(forbidden, 3, 403), (absent, 1, 404)] {
        let error = cli(dir.path(), &server)
            .env_remove("VOLTAGE_EXECUTE")
            .args(currency_from_wallet(wallet))
            .assert()
            .code(code);
        let report: Value = serde_json::from_str(&stderr(&error)).unwrap();
        assert_eq!(report["error"]["detail"]["http_status"], status, "{report}");
    }
    server.verify().await;
}

#[tokio::test]
async fn a_usd_wallet_needs_its_amount_unit_named_and_a_quote() {
    let (server, dir) = fixture().await;
    serve_line_of_credit(&server, WALLET, RESOURCE, ok(json!({"currency": "usd"})), 4).await;
    // A cent is worth more than a sat, so an amount meant in sats would overpay as cents.
    let error = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(currency_from_wallet(WALLET))
        .assert()
        .code(2);
    assert!(stderr(&error).contains("This wallet's currency is usd, so --amount needs --unit"));
    assert!(stderr(&error).contains("Pass --unit cents or --unit usd."));
    // A unit in another currency would denominate the payment wrongly, on a receive too.
    let error = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args([
            "payments", "receive", "--org", ORG, "--env", ENV, "--wallet", WALLET, "--kind",
            "bolt11", "--amount", "2", "--unit", "sats",
        ])
        .assert()
        .code(2);
    assert!(
        stderr(&error).contains("--unit sats is a btc unit, but this wallet's currency is usd")
    );
    assert!(stderr(&error).contains("or --currency to choose another"));
    let mut cents = currency_from_wallet(WALLET);
    cents.extend(["--unit", "cents"]);
    let mut send = cents.clone();
    send.extend(["--quote", RESOURCE]);
    let described = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(&send)
        .assert()
        .code(6);
    let body = json_stdout(&described)["data"]["body"].clone();
    assert_eq!(body["currency"], "usd");
    assert_eq!(
        body["data"]["amount"],
        json!({"currency": "usd", "amount": 250})
    );
    assert!(stderr(&described).contains("Using the wallet's currency, usd"));
    // Without a quote the CLI refuses the USD payment itself, before any write.
    let error = cli(dir.path(), &server)
        .env_remove("VOLTAGE_EXECUTE")
        .args(&cents)
        .assert()
        .code(2);
    let error = stderr(&error);
    assert!(error.contains("A USD payment needs --quote QUOTE_ID (quote_id in --data)"));
    assert!(error.contains("A USD payment needs a wallet on a USD line of credit and a quote"));
    assert!(error.contains("use --currency btc"));
    server.verify().await;
}

#[tokio::test]
async fn local_mistakes_fail_before_the_wallet_is_read_for_its_currency() {
    let (server, dir) = fixture().await;
    // No credential either: a local mistake must not need one.
    let local = |args: &[&str], execute: Option<&str>| {
        let mut cmd = cli(dir.path(), &server);
        cmd.env_remove("VOLTAGE_API_KEY")
            .env_remove("VOLTAGE_EXECUTE");
        if let Some(value) = execute {
            cmd.env("VOLTAGE_EXECUTE", value);
        }
        stderr(&cmd.args(args).assert().code(2))
    };
    let mut receive_flag = currency_from_wallet(WALLET);
    receive_flag.extend(["--kind", "bolt11"]);
    assert!(local(&receive_flag, None).contains("Send commands do not accept receive flags"));
    let mut fee_unit = currency_from_wallet(WALLET);
    fee_unit.extend(["--fee-unit", "sats"]);
    assert!(local(&fee_unit, None).contains("--fee-unit requires --max-fee"));
    let receive = [
        "payments", "receive", "--org", ORG, "--env", ENV, "--wallet", WALLET, "--amount", "2",
    ];
    assert!(local(&receive, None).contains("--kind is required"));
    let misspelled = local(&currency_from_wallet(WALLET), Some("yes"));
    assert!(misspelled.contains("VOLTAGE_EXECUTE must be 1, true, 0, or false"));
    let unscoped: Vec<&str> = currency_from_wallet(WALLET)
        .into_iter()
        .filter(|arg| !matches!(*arg, "--org" | ORG))
        .collect();
    assert!(local(&unscoped, None).contains("--org is required"));
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn checkout_session_reads_send_the_requested_origin() {
    let (server, dir) = fixture().await;
    // The Origin header decides whether a browser-bound session accepts the request.
    respond_once(
        &server,
        route("GET", format!("/checkout/sessions/{RESOURCE}"))
            .and(header("origin", "https://shop.example.test")),
        ok(json!({"id": RESOURCE, "status": "ready"})),
    )
    .await;
    cli(dir.path(), &server)
        .env("VOLTAGE_CHECKOUT_TOKEN", CHECKOUT_KEY)
        .args([
            "checkout",
            "sessions",
            "get",
            RESOURCE,
            "--origin",
            "https://shop.example.test",
        ])
        .assert()
        .success();
    server.verify().await;
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

#[tokio::test(flavor = "multi_thread")]
async fn an_accepted_receive_or_quote_names_the_command_that_shows_it() {
    let (server, dir) = fixture().await;
    route("POST", payments_path())
        .respond_with(accepted())
        .expect(4)
        .mount(&server)
        .await;
    let quotes = format!("/organizations/{ORG}/environments/{ENV}/quotes");
    route("POST", quotes)
        .respond_with(accepted())
        .expect(2)
        .mount(&server)
        .await;
    let receive = |extra: &[&str]| {
        let mut process = human_process_at(dir.path(), &server.uri());
        let mut args = vec!["--wallet", WALLET, "--amount", "1000", "--unit", "sats"];
        args.extend_from_slice(extra);
        process.args(receive_args(&args));
        Command::from_std(process).assert().success()
    };
    let shown = receive(&["--output", "table"]);
    assert!(stdout(&shown).contains("accepted"));
    assert!(stderr(&shown).contains(&format!(
        "hint: Show the invoice or address with `voltage payments get {RESOURCE} --org {ORG} --env {ENV}`, or pass --wait ready next time"
    )));
    // --quiet and machine-readable output leave the hint out.
    assert!(!stderr(&receive(&["--output", "table", "--quiet"])).contains("hint:"));
    assert!(!stderr(&receive(&["--output", "json"])).contains("hint:"));

    // A new ID: the receives' recovery record already holds RESOURCE.
    let quote_id = "55555555-5555-4555-8555-555555555555";
    let mut quote = human_process_at(dir.path(), &server.uri());
    quote.args([
        "quotes",
        "create",
        "--org",
        ORG,
        "--env",
        ENV,
        "--id",
        quote_id,
        "--credit-line",
        WALLET,
        "--network",
        "mutinynet",
        "--amount",
        "10",
        "--unit",
        "usd",
        "--to",
        "btc",
        "--output",
        "table",
    ]);
    let quoted = Command::from_std(quote).assert().success();
    assert!(stderr(&quoted).contains(&format!(
        "hint: Show the quote with `voltage quotes get {quote_id} --org {ORG} --env {ENV}` once it is ready."
    )));

    // A body without an ID gives no ID to name, so there is no hint.
    let body = dir.path().join("quote.json");
    std::fs::write(&body, r#"{"amount":10}"#).unwrap();
    let mut unnamed = human_process_at(dir.path(), &server.uri());
    unnamed.args([
        "quotes", "create", "--org", ORG, "--env", ENV, "--output", "table",
    ]);
    unnamed.arg("--data").arg(format!("@{}", body.display()));
    let unnamed = Command::from_std(unnamed).assert().success();
    assert!(!stderr(&unnamed).contains("hint:"), "{}", stderr(&unnamed));

    // An accepted send creates nothing to show, so it names no next step.
    let mut send = human_process_at(dir.path(), &server.uri());
    send.args([
        "payments",
        "send",
        "--org",
        ORG,
        "--env",
        ENV,
        "--id",
        "66666666-6666-4666-8666-666666666666",
        "--wallet",
        WALLET,
        "--currency",
        "btc",
        "--invoice",
        "lnbc1",
        "--output",
        "table",
    ]);
    let sent = Command::from_std(send).assert().success();
    assert!(stdout(&sent).contains("accepted"));
    assert!(!stderr(&sent).contains("hint:"), "{}", stderr(&sent));
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_get_takes_its_id_as_a_flag_too_but_not_both() {
    let (server, dir) = fixture().await;
    let quote = format!("/organizations/{ORG}/environments/{ENV}/quotes/{RESOURCE}");
    route("GET", quote)
        .respond_with(ok(json!({"id": RESOURCE})))
        .expect(1)
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args([
            "quotes", "get", "--org", ORG, "--env", ENV, "--id", RESOURCE,
        ])
        .assert()
        .success();
    assert_eq!(json_stdout(&result)["data"]["id"], RESOURCE);
    let both = cli(dir.path(), &server)
        .args([
            "quotes", "get", RESOURCE, "--org", ORG, "--env", ENV, "--id", RESOURCE,
        ])
        .assert()
        .code(2);
    assert!(stderr(&both).contains("cannot be used with"));
    let neither = cli(dir.path(), &server)
        .args(["quotes", "get", "--org", ORG, "--env", ENV])
        .assert()
        .code(2);
    assert!(stderr(&neither).contains("<QUOTE_ID|--id <ID>>"));
    let help = cli(dir.path(), &server)
        .args(["quotes", "get", "-h"])
        .assert()
        .success();
    assert!(stdout(&help).contains("Usage: voltage quotes get [OPTIONS] <QUOTE_ID|--id <ID>>\n"));
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
async fn api_rejections_lead_with_the_reason_and_keep_the_response_for_json() {
    let (server, dir) = fixture().await;
    let rejection = json!({"error": {
        "type": "quote_required", "code": "required",
        "detail": "Quote is required for payment",
        "context": {"field": "quote_id", "required": true}
    }});
    route("POST", payments_path())
        .respond_with(ResponseTemplate::new(400).set_body_json(rejection.clone()))
        .expect(2)
        .mount(&server)
        .await;
    let mut human = human_process_at(dir.path(), &server.uri());
    human.args(receive_args(&[
        "--wallet", WALLET, "--amount", "1000", "--unit", "sats", "--output", "table",
    ]));
    let error = stderr(&Command::from_std(human).assert().code(1));
    let report = error
        .split_once("error: ")
        .map(|(_, report)| report)
        .unwrap_or_default();
    assert_eq!(
        report,
        "Voltage API returned HTTP 400: Quote is required for payment\n\
         hint: A wallet on a USD line of credit needs a quote for each payment. Create one with \
         `voltage quotes create`, wait until `voltage quotes get QUOTE_ID` shows it, then pass \
         --quote QUOTE_ID. A wallet on a BTC line of credit needs no quote.\n\
         details:\n  \
         code              required\n  \
         context.field     quote_id\n  \
         context.required  true\n  \
         type              quote_required\n",
        "{error}"
    );

    let mut machine = process_at(dir.path(), &server.uri());
    machine.args(receive_args(&[
        "--wallet", WALLET, "--amount", "1000", "--unit", "sats",
    ]));
    let output = Command::from_std(machine).assert().code(1);
    let report = error_report(&output.get_output().stderr);
    assert_eq!(report["error"]["detail"]["data"], rejection);
}

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
async fn payment_check_filter_is_optional_and_survives_cursor_pages() {
    for value in [None, Some("true"), Some("false")] {
        let (server, dir) = fixture().await;
        for cursor in [None, Some("next")] {
            let mut matcher = route("GET", payments_path())
                .and(query_param("pagination", "cursor"))
                .and(query_param("statuses[]", "approved"));
            matcher = match value {
                Some(value) => matcher.and(query_param("check_only", value)),
                None => matcher.and(query_param_is_missing("check_only")),
            };
            matcher = match cursor {
                Some(cursor) => matcher.and(query_param("cursor", cursor)),
                None => matcher.and(query_param_is_missing("cursor")),
            };
            respond_once(
                &server,
                matcher,
                ok(json!({
                    "items": [], "has_more": cursor.is_none(),
                    "next_cursor": if cursor.is_none() { Some("next") } else { None }
                })),
            )
            .await;
        }
        let mut command = cli(dir.path(), &server);
        command.args([
            "payments",
            "list",
            "--org",
            ORG,
            "--env",
            ENV,
            "--statuses",
            "approved",
            "--all",
        ]);
        if let Some(value) = value {
            command.args(["--check-only", value]);
        }
        command.assert().success();
        server.verify().await;
        let before = server.received_requests().await.unwrap().len();
        cli(dir.path(), &server)
            .args([
                "payments",
                "list",
                "--org",
                ORG,
                "--env",
                ENV,
                "--check-only",
                "invalid",
            ])
            .assert()
            .code(2);
        assert_eq!(server.received_requests().await.unwrap().len(), before);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn payment_ledger_omits_event_type_and_rejects_explicit_payment() {
    let (server, dir) = fixture().await;
    respond_once(
        &server,
        route(
            "GET",
            format!("/organizations/{ORG}/wallets/{WALLET}/ledger"),
        )
        .and(query_param_is_missing("event_type")),
        ok(json!({"items": [{"id": RESOURCE}], "total": 1})),
    )
    .await;
    let result = cli(dir.path(), &server)
        .args(["wallets", "ledger", "list", WALLET, "--org", ORG])
        .assert()
        .success();
    assert_eq!(json_stdout(&result)["data"]["items"][0]["id"], RESOURCE);
    server.verify().await;
    cli(dir.path(), &server)
        .args([
            "wallets",
            "ledger",
            "list",
            WALLET,
            "--org",
            ORG,
            "--event-type",
            "payment",
        ])
        .assert()
        .code(2);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn adjustment_ledger_keeps_filters_when_paging_and_rejects_unknown_event_types() {
    let (server, dir) = fixture().await;
    for offset in [None, Some("1")] {
        let mut matcher = route(
            "GET",
            format!("/organizations/{ORG}/wallets/{WALLET}/ledger"),
        )
        .and(query_param("event_type", "adjustment"))
        .and(query_param("start_date", "2026-10-01T00:00:00Z"))
        .and(query_param("end_date", "2026-10-22T00:00:00Z"))
        .and(query_param("limit", "1"));
        matcher = match offset {
            Some(offset) => matcher.and(query_param("offset", offset)),
            None => matcher.and(query_param_is_missing("offset")),
        };
        respond_once(
            &server,
            matcher,
            ok(json!({
                "items": [{"id": RESOURCE}], "limit": 1,
                "offset": if offset.is_none() { 0 } else { 1 }, "total": 2
            })),
        )
        .await;
    }
    cli(dir.path(), &server)
        .args([
            "wallets",
            "ledger",
            "list",
            WALLET,
            "--org",
            ORG,
            "--event-type",
            "adjustment",
            "--start-date",
            "2026-10-01T00:00:00Z",
            "--end-date",
            "2026-10-22T00:00:00Z",
            "--limit",
            "1",
            "--all",
        ])
        .assert()
        .success();
    server.verify().await;
    let before = server.received_requests().await.unwrap().len();
    cli(dir.path(), &server)
        .args([
            "wallets",
            "ledger",
            "list",
            WALLET,
            "--org",
            ORG,
            "--event-type",
            "invented",
        ])
        .assert()
        .code(2);
    assert_eq!(server.received_requests().await.unwrap().len(), before);
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
async fn api_reasons_that_echo_the_key_are_redacted_in_json_and_human_output() {
    let (server, dir) = fixture().await;
    route("GET", wallets_path(ORG))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "type": "invalid_request", "detail": format!("Bad key {ACCOUNT_KEY}")
        }})))
        .mount(&server)
        .await;
    let machine = cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .code(1);
    let error = json_error(&machine);
    assert_eq!(
        error["error"]["message"],
        "Voltage API returned HTTP 400: Bad key [REDACTED]"
    );
    assert_eq!(
        error["error"]["detail"]["data"]["error"]["detail"],
        "Bad key [REDACTED]"
    );
    assert!(!stderr(&machine).contains(ACCOUNT_KEY));

    let mut human = human_process_at(dir.path(), &server.uri());
    human.args(["wallets", "list", "--org", ORG, "--output", "table"]);
    let human = stderr(&Command::from_std(human).assert().code(1));
    assert!(
        human.starts_with("error: Voltage API returned HTTP 400: Bad key [REDACTED]\n"),
        "{human}"
    );
    assert!(!human.contains(ACCOUNT_KEY), "{human}");
}

#[tokio::test(flavor = "multi_thread")]
async fn server_error_text_stays_out_of_the_message_and_is_redacted() {
    let (server, dir) = fixture().await;
    route("GET", wallets_path(ORG))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": {
            "type": "internal", "detail": format!("query failed for {ACCOUNT_KEY}")
        }})))
        .mount(&server)
        .await;
    let machine = cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .failure();
    let error = json_error(&machine);
    assert_eq!(error["error"]["message"], "Voltage API returned HTTP 500");
    assert_eq!(
        error["error"]["detail"]["data"]["error"]["detail"],
        "query failed for [REDACTED]"
    );

    let mut human = human_process_at(dir.path(), &server.uri());
    human.args(["wallets", "list", "--org", ORG, "--output", "table"]);
    let human = stderr(&Command::from_std(human).assert().failure());
    assert!(
        human.starts_with("error: Voltage API returned HTTP 500\n"),
        "{human}"
    );
    assert!(human.contains("query failed for [REDACTED]"), "{human}");
    assert!(!human.contains(ACCOUNT_KEY), "{human}");
}

/// The JSON error report a failed `--json` command writes as its last stderr line.
fn json_error(assertion: &Assert) -> Value {
    error_report(&assertion.get_output().stderr)
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_credentials_name_the_command_that_checks_them() {
    for (status, next) in [(401, "voltage auth status"), (403, "voltage context")] {
        let (server, dir) = fixture().await;
        seeded_api_key(dir.path(), "saved", ACCOUNT_KEY);
        route("GET", wallets_path(ORG))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        let result = cli(dir.path(), &server)
            .args(["wallets", "list", "--account", "saved", "--org", ORG])
            .assert()
            .code(3);
        let error = json_error(&result);
        assert!(
            error["error"]["hint"].as_str().unwrap().contains(next),
            "{error}"
        );
        // A saved credential is the one the user chose, so no sources are reported.
        assert!(error["error"]["detail"].get("credential_sources").is_none());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forbidden_environment_key_names_where_it_and_the_organization_came_from() {
    for (from_flag, organization, named) in [
        (true, "flag", " and the organization from --org"),
        (
            false,
            "environment",
            " and the organization from VOLTAGE_ORGANIZATION_ID",
        ),
    ] {
        let (server, dir) = fixture().await;
        route("GET", wallets_path(ORG))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": {
                "type": "access_rights", "detail": "No Access"
            }})))
            .mount(&server)
            .await;
        let mut command = cli(dir.path(), &server);
        command.args(["wallets", "list"]);
        if from_flag {
            command.args(["--org", ORG]);
        } else {
            command.env("VOLTAGE_ORGANIZATION_ID", ORG);
        }
        let error = json_error(&command.assert().code(3));
        assert_eq!(
            error["error"]["hint"],
            format!(
                "The API key came from VOLTAGE_API_KEY{named}. Check that the key can reach the organization, environment, and wallet in use, which `voltage context` shows, or pass --account NAME or unset VOLTAGE_API_KEY to use a saved login."
            )
        );
        assert_eq!(
            error["error"]["detail"]["credential_sources"],
            json!({"credential": "environment", "organization": organization})
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unauthorized_environment_key_reports_no_sources() {
    let (server, dir) = fixture().await;
    route("GET", wallets_path(ORG))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .code(3);
    let error = json_error(&result);
    assert_eq!(
        error["error"]["hint"],
        "Check the key with `voltage auth status`; the API does not accept it."
    );
    assert!(error["error"]["detail"].get("credential_sources").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forbidden_environment_key_without_a_json_body_still_names_its_sources() {
    let (server, dir) = fixture().await;
    route("GET", wallets_path(ORG))
        .respond_with(ResponseTemplate::new(403).set_body_raw("<html>denied</html>", "text/html"))
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .code(3);
    let error = json_error(&result);
    assert_eq!(
        error["error"]["message"],
        "HTTP 403: non-JSON response omitted"
    );
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .starts_with("The API key came from VOLTAGE_API_KEY and the organization from --org."),
        "{error}"
    );
    assert_eq!(
        error["error"]["detail"]["credential_sources"],
        json!({"credential": "environment", "organization": "flag"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unrecognized_environment_key_on_a_403_reports_no_sources() {
    let (server, dir) = fixture().await;
    route("GET", wallets_path(ORG))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error": {
            "type": "missing_credentials", "detail": "Missing credential"
        }})))
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .code(3);
    let error = json_error(&result);
    // The rejection is about the key itself, so the hint and the sources agree with a 401.
    assert_eq!(
        error["error"]["hint"],
        "Check the key with `voltage auth status`; the API does not accept it."
    );
    assert!(error["error"]["detail"].get("credential_sources").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_login_names_the_command_that_renews_it() {
    // An expiring login fails at refresh; a session revoked before its access token
    // expires fails at the organization token exchange. Both name the same fix.
    for expiring in [true, false] {
        let (server, dir) = fixture().await;
        seeded_login(dir.path(), &server, expiring);
        route("POST", "/oauth/token")
            .respond_with(
                ResponseTemplate::new(400).set_body_json(json!({"error":"invalid_grant"})),
            )
            .mount(&server)
            .await;
        let result = cli(dir.path(), &server)
            .args(["wallets", "list", "--account", LOGIN_NAME, "--org", ORG])
            .assert()
            .code(3);
        let error = json_error(&result);
        assert_eq!(
            error["error"]["message"],
            format!("The saved login for {LOGIN_NAME} has expired or was revoked")
        );
        // The mock auth service is not the default one, so the hint names it again.
        assert_eq!(
            error["error"]["hint"],
            format!(
                "Sign in again with `voltage login --account {LOGIN_NAME} --auth-url {}`.",
                server.uri()
            ),
            "expiring={expiring}"
        );
        assert_eq!(
            error["error"]["detail"],
            json!({"credential_problem": "login_expired"}),
            "expiring={expiring}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unrecognized_key_names_the_command_that_checks_it() {
    let (server, dir) = fixture().await;
    route("GET", wallets_path(ORG))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {
            "type": "missing_credentials", "detail": "Missing credential"
        }})))
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .code(1);
    let error = json_error(&result);
    assert_eq!(
        error["error"]["message"],
        "Voltage API returned HTTP 400: Missing credential"
    );
    assert_eq!(
        error["error"]["hint"],
        "Check the key with `voltage auth status`; the API does not accept it."
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejections_without_a_json_body_still_name_the_next_step() {
    let (server, dir) = fixture().await;
    route("GET", wallets_path(ORG))
        .respond_with(ResponseTemplate::new(401).set_body_string("<html>denied</html>"))
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .code(3);
    let error = json_error(&result);
    assert_eq!(
        error["error"]["message"],
        "HTTP 401: non-JSON response omitted"
    );
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("voltage auth status"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn human_errors_keep_a_reason_and_request_id_the_message_lacks() {
    let (server, dir) = fixture().await;
    route("GET", wallets_path(ORG))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"type": null, "detail": "Wallet is frozen"},
            "request_id": "req-123"
        })))
        .mount(&server)
        .await;
    let mut human = human_process_at(dir.path(), &server.uri());
    human.args(["wallets", "list", "--org", ORG, "--output", "table"]);
    let shown = stderr(&Command::from_std(human).assert().code(1));
    // The untyped error leaves the reason out of the message, so the details keep it.
    assert!(
        shown.starts_with("error: Voltage API returned HTTP 400\n"),
        "{shown}"
    );
    assert!(shown.contains("Wallet is frozen"), "{shown}");
    assert!(shown.contains("req-123"), "{shown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_and_profile_errors_list_the_saved_names() {
    let (server, dir) = fixture().await;
    let mut settings = seeded_api_key(dir.path(), "first", "first-key");
    settings.config.accounts.insert(
        "second".into(),
        Account {
            store: CredentialStore::File,
            kind: AccountKind::ApiKey,
            email: None,
            organization_id: Some(uuid(ORG)),
            environment_id: Some(uuid(ENV)),
            auth_url: config::AUTH_URL.into(),
        },
    );
    settings.save().unwrap();
    let hint = |args: &[&str], code: i32| {
        let result = cli(dir.path(), &server)
            .env_remove("VOLTAGE_API_KEY")
            .args(["wallets", "list", "--org", ORG])
            .args(args)
            .assert()
            .code(code);
        json_error(&result)["error"]["hint"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(hint(&[], 2).starts_with("Saved credentials: first, second."));
    assert_eq!(
        hint(&["--account", "third"], 3),
        "Saved credentials: first, second."
    );
    assert!(hint(&["--profile", "missing"], 2).starts_with("No profiles are saved"));
    // `first` is bound to ORG, so it cannot reach another organization.
    let elsewhere = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(["wallets", "list", "--org", RESOURCE, "--account", "first"])
        .assert()
        .code(2);
    assert_eq!(
        json_error(&elsewhere)["error"]["hint"],
        "Saved credentials: first, second. Select a key for this scope or a browser login with --account NAME."
    );

    settings.config.profiles.insert(
        "main".into(),
        Profile {
            organization_id: uuid(ORG),
            environment_id: uuid(ENV),
            account: "first".into(),
        },
    );
    settings.save().unwrap();
    assert_eq!(hint(&["--profile", "missing"], 2), "Saved profiles: main.");
    for verb in ["get", "delete"] {
        let result = cli(dir.path(), &server)
            .args(["profiles", verb, "missing"])
            .assert()
            .code(2);
        let error = json_error(&result);
        assert_eq!(error["error"]["message"], "Unknown profile missing");
        assert_eq!(error["error"]["hint"], "Saved profiles: main.");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn credential_errors_without_saved_credentials_name_how_to_add_one() {
    let (server, dir) = fixture().await;
    let unknown = cli(dir.path(), &server)
        .env_remove("VOLTAGE_API_KEY")
        .args(["wallets", "list", "--org", ORG, "--account", "missing"])
        .assert()
        .code(3);
    assert_eq!(
        json_error(&unknown)["error"]["hint"],
        "No credentials are saved; sign in with `voltage login` or import a key with `voltage auth import-key`."
    );
    let empty = cli(dir.path(), &server)
        .env("VOLTAGE_API_KEY", " ")
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .code(3);
    assert_eq!(
        json_error(&empty)["error"]["hint"],
        "Unset VOLTAGE_API_KEY to use a saved credential, or set it to a key."
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_scope_errors_name_the_command_that_lists_it() {
    let (server, dir) = fixture().await;
    let error = |args: &[&str]| {
        let result = cli(dir.path(), &server).args(args).assert().code(2);
        json_error(&result)["error"].clone()
    };
    let webhook = error(&["webhooks", "deliveries", "list", "--org", ORG, "--env", ENV]);
    assert_eq!(
        webhook["message"],
        "--webhook is required for webhooks deliveries list"
    );
    assert_eq!(
        webhook["hint"],
        "List webhooks with `voltage webhooks list`, then pass --webhook ID."
    );
    for envs in [&[][..], &["--env", ENV, "--env", RESOURCE][..]] {
        let mut args = vec!["payments", "get", RESOURCE, "--org", ORG];
        args.extend_from_slice(envs);
        let environment = error(&args);
        assert_eq!(
            environment["message"],
            "Exactly one --env is required for this operation"
        );
        assert!(
            environment["hint"]
                .as_str()
                .unwrap()
                .starts_with("List environments with `voltage environments list --org ORG_ID`"),
            "{environment}"
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_with_an_api_key_says_which_key_was_used() {
    let (server, dir) = fixture().await;
    let result = cli(dir.path(), &server)
        .args(["organizations", "list"])
        .assert()
        .code(3);
    let error = json_error(&result);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("VOLTAGE_API_KEY from the environment"),
        "{error}"
    );

    // A saved API key selected without the ambient one is named instead.
    seeded_api_key(dir.path(), "saved", "saved-key");
    for (args, command) in [
        (&["organizations", "list"][..], "organizations list"),
        (
            &["environments", "list", "--org", ORG][..],
            "environments list",
        ),
    ] {
        let result = cli(dir.path(), &server)
            .env_remove("VOLTAGE_API_KEY")
            .args(args)
            .assert()
            .code(3);
        let error = json_error(&result);
        assert_eq!(
            error["error"]["message"],
            format!(
                "{command} needs a browser login, but the selected credential saved is an API key"
            )
        );
        assert_eq!(
            error["error"]["hint"],
            "Select a browser login with --account NAME, or sign in with `voltage login`."
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_checkout_tokens_point_to_the_checkout_token_not_the_account() {
    let (server, dir) = fixture().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .env("VOLTAGE_CHECKOUT_TOKEN", CHECKOUT_KEY)
        .args(["checkout", "sessions", "get", RESOURCE])
        .assert()
        .code(3);
    let hint = json_error(&result)["error"]["hint"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(hint.contains("VOLTAGE_CHECKOUT_TOKEN"), "{hint}");
    assert!(!hint.contains("voltage auth status"), "{hint}");
    assert!(!hint.contains("voltage login"), "{hint}");
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
    let running = KillOnDrop::new(&child);
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
    let output = exit_within(
        child,
        running,
        Duration::from_secs(5),
        "Ctrl-C did not end the checkout event stream",
    )
    .await;
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

/// Run a command with stdin and stderr on a pseudo-terminal and stdout piped. For each step,
/// once the terminal shows its prompt after the previous step's, type its keys. A step with
/// no keys sends SIGINT instead, as the terminal does to its foreground process.
#[cfg(unix)]
async fn on_a_terminal(
    dir: &Path,
    server: &MockServer,
    args: &[&str],
    steps: &[(&'static str, &'static [u8])],
) -> TerminalRun {
    on_a_terminal_with(dir, server, args, steps, |_| {}).await
}

/// How long a terminal step may take: until its prompt is shown, and until the command exits.
/// A device login waits out its 5-second poll interval before its first prompt, and a loaded
/// machine is slower still. A passing step finishes well within this; it only bounds a failure.
#[cfg(unix)]
const TERMINAL_STEP: Duration = Duration::from_secs(30);

/// `on_a_terminal`, calling `meanwhile` with each step's index once its prompt is shown and
/// before its keys are typed, as another command could run while a prompt is open.
#[cfg(unix)]
async fn on_a_terminal_with(
    dir: &Path,
    server: &MockServer,
    args: &[&str],
    steps: &[(&'static str, &'static [u8])],
    mut meanwhile: impl FnMut(usize),
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
    let running = KillOnDrop::new(&child);
    let mut prompts = steps
        .iter()
        .map(|&(prompt, _)| prompt.as_bytes())
        .collect::<Vec<_>>();
    prompts.reverse();
    let (prompted, mut shown_prompt) = tokio::sync::mpsc::unbounded_channel();
    let drain = tokio::task::spawn_blocking(move || {
        let (mut shown, mut seen) = (Vec::new(), 0);
        let mut chunk = [0; 4096];
        while let Ok(read @ 1..) = controller.read(&mut chunk) {
            shown.extend_from_slice(&chunk[..read]);
            while let Some(prompt) = prompts.last() {
                let Some(at) = shown[seen..]
                    .windows(prompt.len())
                    .position(|w| w == *prompt)
                else {
                    break;
                };
                seen += at + prompt.len();
                prompts.pop();
                let _ = prompted.send(());
            }
        }
        String::from_utf8_lossy(&shown).into_owned()
    });
    for (index, &(_, keys)) in steps.iter().enumerate() {
        tokio::time::timeout(TERMINAL_STEP, shown_prompt.recv())
            .await
            .unwrap()
            .unwrap();
        meanwhile(index);
        if keys.is_empty() {
            interrupt(&child);
        } else {
            (&observer).write_all(keys).unwrap();
        }
    }
    let output = exit_within(
        child,
        running,
        TERMINAL_STEP,
        "the command on the terminal did not exit",
    )
    .await;
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
            {"id": RESOURCE, "name": "ops", "balances": [{"currency": "usd"}]},
            {"id": WALLET, "name": "Payroll", "balances": [{"currency": "btc"}]}
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

    // Each wallet shows its currency, typing filters on it, and Enter picks; the change is
    // still only described.
    let picked = on_a_terminal(dir.path(), &server, &send, &[("Wallet:", b"btc\r")]).await;
    assert_eq!(picked.code, Some(6), "{}", picked.shown);
    assert!(
        picked.shown.contains(&format!("ops  USD  {RESOURCE}")),
        "{}",
        picked.shown
    );
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
    let escaped = on_a_terminal(dir.path(), &server, &send, &[("Wallet:", b"\x1b")]).await;
    assert_eq!(escaped.code, Some(2), "{}", escaped.shown);
    assert!(
        escaped
            .shown
            .contains("--wallet is required for payment creation")
    );
    assert_eq!(escaped.modes.0, escaped.modes.1);

    // A typed Ctrl-C reaches the picker as a key in raw mode and still exits with 130.
    let interrupted = on_a_terminal(dir.path(), &server, &send, &[("Wallet:", b"\x03")]).await;
    assert_eq!(interrupted.code, Some(130), "{}", interrupted.shown);
    assert!(
        interrupted
            .shown
            .contains("Interrupted before any resource change was submitted")
    );
    assert_eq!(interrupted.modes.0, interrupted.modes.1);
    assert_eq!(listings().await, 3);

    // JSON output and --no-input never ask. Without a picker, the BOLT11 send lists the
    // wallets once to say which could pay; these report no network, so none qualifies.
    for extra in ["--json", "--no-input"] {
        let mut args = send.to_vec();
        args.push(extra);
        let run = on_a_terminal(dir.path(), &server, &args, &[]).await;
        assert_eq!(run.code, Some(2), "{extra}: {}", run.shown);
        assert!(
            run.shown
                .contains("--wallet is required for payment creation"),
            "{extra}"
        );
        assert!(run.shown.contains("is on an unknown network"), "{extra}");
        assert!(!run.shown.contains("Wallet:"), "{extra}");
    }
    assert_eq!(listings().await, 5);

    // An empty list says so instead of opening the picker.
    Mock::given(method("GET"))
        .and(path(wallets_path(ORG)))
        .and(query_param("environment_id", RESOURCE))
        .respond_with(ok(json!([])))
        .mount(&server)
        .await;
    let mut args = send.to_vec();
    args[5] = RESOURCE;
    let empty = on_a_terminal(dir.path(), &server, &args, &[]).await;
    assert_eq!(empty.code, Some(2), "{}", empty.shown);
    assert!(
        empty.shown.contains("No wallets to choose from."),
        "{}",
        empty.shown
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_api_key_without_an_organization_keeps_the_original_error() {
    let (server, dir) = fixture().await;
    let run = on_a_terminal(
        dir.path(),
        &server,
        &["wallets", "list", "--output", "table"],
        &[],
    )
    .await;
    assert_eq!(run.code, Some(2), "{}", run.shown);
    assert!(
        run.shown.contains("--org is required for wallets list"),
        "{}",
        run.shown
    );
    assert!(
        run.shown.contains("voltage organizations list"),
        "{}",
        run.shown
    );

    // A payment needs a wallet, listed within an organization the key cannot list, so the
    // person is told why nothing is offered.
    let receive = [
        "payments",
        "receive",
        "--currency",
        "btc",
        "--kind",
        "bolt11",
        "--amount",
        "1000",
        "--output",
        "table",
    ];
    let run = on_a_terminal(dir.path(), &server, &receive, &[]).await;
    assert_eq!(run.code, Some(2), "{}", run.shown);
    assert!(
        run.shown
            .contains("An API key cannot list organizations, so there is nothing to pick."),
        "{}",
        run.shown
    );
    assert!(
        run.shown
            .contains("--wallet is required for payment creation"),
        "{}",
        run.shown
    );

    // With an organization, the key still cannot list environments, and says so.
    let mut args = receive.to_vec();
    args.extend(["--org", ORG]);
    let run = on_a_terminal(dir.path(), &server, &args, &[]).await;
    assert_eq!(run.code, Some(2), "{}", run.shown);
    assert!(
        run.shown
            .contains("An API key cannot list environments, so there is nothing to pick."),
        "{}",
        run.shown
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_credential_is_reported_before_the_missing_scope() {
    let (server, dir) = fixture().await;
    let run = on_a_terminal(
        dir.path(),
        &server,
        &[
            "payments",
            "receive",
            "--account",
            "nobody",
            "--currency",
            "btc",
            "--kind",
            "bolt11",
            "--amount",
            "1000",
            "--output",
            "table",
        ],
        &[],
    )
    .await;
    assert_eq!(run.code, Some(3), "{}", run.shown);
    assert!(
        run.shown.contains("Unknown credential nobody"),
        "{}",
        run.shown
    );
    assert!(!run.shown.contains("--wallet is required"), "{}", run.shown);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_takes_the_only_organization_and_environment_without_asking() {
    let auth = MockServer::start().await;
    let (api, dir) = fixture().await;
    let mut settings = seeded_login(dir.path(), &auth, false);
    let organizations = json!({"organizations": [{"id": ORG, "name": "Acme"}]});
    respond(&auth, route("GET", "/users/current"), ok(organizations), 2).await;
    organization_exchange(&auth, LOGIN_ACCESS, ORG, "picker-access", 2).await;
    let environments = route("GET", format!("/organizations/{ORG}/environments"))
        .and(header("authorization", "Bearer picker-access"));
    respond(
        &auth,
        environments,
        ok(json!([{"id": ENV, "name": "prod"}])),
        2,
    )
    .await;

    // A profile needs both. Saving one is what the hint would suggest, so it is left out.
    let args = [
        "profiles",
        "create",
        "prod",
        "--account",
        LOGIN_NAME,
        "--output",
        "table",
    ];
    // The only organization and environment are taken without asking, and say so.
    let run = on_a_terminal(dir.path(), &api, &args, &[]).await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    for taken in [
        format!("Organization: Acme  {ORG} (the only one)"),
        format!("Environment: prod  {ENV} (the only one)"),
    ] {
        assert!(run.shown.contains(&taken), "{}", run.shown);
    }
    assert!(
        run.shown
            .contains(&format!("Next time, pass --org {ORG} --env {ENV}.")),
        "{}",
        run.shown
    );
    settings.reload().unwrap();
    let profile = &settings.config.profiles["prod"];
    assert_eq!(
        (profile.organization_id, profile.environment_id),
        (uuid(ORG), uuid(ENV))
    );

    // Listing environments needs only the organization, so nothing more is asked.
    let args = [
        "environments",
        "list",
        "--account",
        LOGIN_NAME,
        "--output",
        "table",
    ];
    let run = on_a_terminal(dir.path(), &api, &args, &[]).await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    assert!(!run.shown.contains("Environment:"), "{}", run.shown);
    auth.verify().await;
}

/// `profiles create` with a login, which asks for an organization and then an environment.
#[cfg(unix)]
const CREATE_PROFILE: [&str; 7] = [
    "profiles",
    "create",
    "prod",
    "--account",
    LOGIN_NAME,
    "--output",
    "table",
];

/// A listing or exchange that fails during a pick exits with its own error, not the missing
/// scope error the pick would have left, and suggests no flags.
#[cfg(unix)]
fn assert_failed_pick(run: &TerminalRun, error: &str) {
    assert_eq!(run.code, Some(3), "{}", run.shown);
    assert!(run.shown.contains(error), "{}", run.shown);
    assert!(!run.shown.contains("is required"), "{}", run.shown);
    assert!(!run.shown.contains("Next time"), "{}", run.shown);
    assert_eq!(run.modes.0, run.modes.1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_listing_or_exchange_ends_the_pick_with_its_own_error() {
    let (api, dir) = fixture().await;
    let organizations = || ok(json!({"organizations": [{"id": ORG, "name": "Acme"}]}));
    let environments = format!("/organizations/{ORG}/environments");

    // The auth service fails to list organizations: nothing is offered.
    let auth = MockServer::start().await;
    seeded_login(dir.path(), &auth, false);
    respond_once(
        &auth,
        route("GET", "/users/current"),
        ResponseTemplate::new(500),
    )
    .await;
    let run = on_a_terminal(dir.path(), &api, &CREATE_PROFILE, &[]).await;
    assert_failed_pick(&run, "Authentication failed (HTTP 500");
    assert!(!run.shown.contains("Organization:"), "{}", run.shown);
    auth.verify().await;

    // The organization token exchange is rejected after the pick.
    let auth = MockServer::start().await;
    seeded_login(dir.path(), &auth, false);
    respond_once(&auth, route("GET", "/users/current"), organizations()).await;
    respond_once(
        &auth,
        route("POST", "/oauth/token"),
        ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})),
    )
    .await;
    let run = on_a_terminal(dir.path(), &api, &CREATE_PROFILE, &[]).await;
    assert_failed_pick(
        &run,
        "The saved login for person has expired or was revoked",
    );
    auth.verify().await;

    // The auth service fails to list environments in the picked organization.
    let auth = MockServer::start().await;
    seeded_login(dir.path(), &auth, false);
    respond_once(&auth, route("GET", "/users/current"), organizations()).await;
    organization_exchange(&auth, LOGIN_ACCESS, ORG, "picker-access", 1).await;
    respond_once(
        &auth,
        route("GET", environments.as_str()),
        ResponseTemplate::new(500),
    )
    .await;
    let run = on_a_terminal(dir.path(), &api, &CREATE_PROFILE, &[]).await;
    assert_failed_pick(&run, "Authentication failed (HTTP 500");
    assert!(!run.shown.contains("Environment:"), "{}", run.shown);
    auth.verify().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn esc_at_the_organization_or_environment_picker_keeps_the_missing_scope_error() {
    let auth = MockServer::start().await;
    let (api, dir) = fixture().await;
    seeded_login(dir.path(), &auth, false);
    // Two of each, so each picker asks rather than taking the only one.
    let organizations = json!({"organizations": [
        {"id": ORG, "name": "Acme"}, {"id": OTHER_ORG, "name": "Beta"}
    ]});
    respond(&auth, route("GET", "/users/current"), ok(organizations), 2).await;
    organization_exchange(&auth, LOGIN_ACCESS, ORG, "picker-access", 1).await;
    respond_once(
        &auth,
        route("GET", format!("/organizations/{ORG}/environments")),
        ok(json!([{"id": ENV, "name": "prod"}, {"id": RESOURCE, "name": "staging"}])),
    )
    .await;

    let run = on_a_terminal(
        dir.path(),
        &api,
        &CREATE_PROFILE,
        &[("Organization:", b"\x1b")],
    )
    .await;
    assert_eq!(run.code, Some(2), "{}", run.shown);
    assert!(run.shown.contains("--org is required"), "{}", run.shown);
    assert!(!run.shown.contains("Environment:"), "{}", run.shown);
    assert!(!run.shown.contains("Next time"), "{}", run.shown);
    assert_eq!(run.modes.0, run.modes.1);

    let steps: &[(&str, &[u8])] = &[("Organization:", b"\r"), ("Environment:", b"\x1b")];
    let run = on_a_terminal(dir.path(), &api, &CREATE_PROFILE, steps).await;
    assert_eq!(run.code, Some(2), "{}", run.shown);
    assert!(
        run.shown
            .contains("Exactly one --env is required for this operation"),
        "{}",
        run.shown
    );
    // The organization picked before Esc is still suggested.
    assert!(
        run.shown.contains(&format!("Next time, pass --org {ORG}.")),
        "{}",
        run.shown
    );
    assert_eq!(run.modes.0, run.modes.1);
    auth.verify().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_wallet_listing_or_two_environments_end_the_pick_with_their_own_error() {
    let (server, dir) = fixture().await;
    respond_once(
        &server,
        route("GET", wallets_path(ORG)).and(query_param("environment_id", ENV)),
        ResponseTemplate::new(403).set_body_json(json!({"message": "forbidden"})),
    )
    .await;
    let mut send = vec![
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
    let run = on_a_terminal(dir.path(), &server, &send, &[]).await;
    assert_failed_pick(&run, "Voltage API returned HTTP 403");
    server.verify().await;

    send.extend(["--env", RESOURCE]);
    let run = on_a_terminal(dir.path(), &server, &send, &[]).await;
    // Two environments are refused before any listing, as `wallets list` refuses them.
    assert_eq!(run.code, Some(2), "{}", run.shown);
    assert!(
        run.shown
            .contains("This operation accepts only one environment filter"),
        "{}",
        run.shown
    );
    assert!(!run.shown.contains("Wallet:"), "{}", run.shown);
    server.verify().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signal_at_the_picker_restores_the_terminal() {
    let (server, dir) = fixture().await;
    Mock::given(method("GET"))
        .and(path(wallets_path(ORG)))
        .respond_with(ok(json!([{"id": WALLET, "name": "Payroll"}])))
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
    // SIGINT ends the command while the picker's read is still blocked, so the picker cannot
    // turn bracketed paste off itself; the guard does.
    let run = on_a_terminal(dir.path(), &server, &send, &[("Wallet:", b"")]).await;
    assert_eq!(run.code, Some(130), "{}", run.shown);
    let (_, after) = run.shown.split_once("Wallet:").unwrap();
    assert!(after.contains("\x1b[?2004l\x1b[?25h"), "{after:?}");
    assert_eq!(run.modes.0, run.modes.1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_payment_wait_on_a_terminal_shows_how_long_it_has_waited() {
    let (server, dir) = fixture().await;
    route("GET", payment_path())
        .respond_with(in_order(vec![
            ok(json!({"id": RESOURCE, "status": "sending"})),
            ok(json!({"id": RESOURCE, "status": "completed"})),
        ]))
        .expect(2)
        .mount(&server)
        .await;
    let run = on_a_terminal(
        dir.path(),
        &server,
        &[
            "payments",
            "get",
            RESOURCE,
            "--org",
            ORG,
            "--env",
            ENV,
            "--wait",
            "completed",
        ],
        &[],
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    assert!(
        run.shown
            .contains("Waiting for the payment to be completed... 0s"),
        "{}",
        run.shown
    );
    server.verify().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn device_approval_on_a_terminal_shows_how_long_it_has_waited() {
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
            "access_token":"login-access-secret","refresh_token":"login-refresh-secret",
            "token_type":"Bearer","expires_in":600
        })),
    )
    .await;
    respond_once(
        &server,
        route("GET", "/users/current"),
        ok(json!({"id":RESOURCE,"email":LOGIN_EMAIL})),
    )
    .await;
    let uri = server.uri();
    let args = [
        "login",
        "--auth-url",
        &uri,
        "--no-browser",
        "--account",
        LOGIN_NAME,
        "--credential-store",
        "file",
    ];
    let run = on_a_terminal(dir.path(), &server, &args, &[]).await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    assert!(
        run.shown.contains("Waiting for device approval... 0s"),
        "{}",
        run.shown
    );
    server.verify().await;
}

mod examples {
    include!("support/examples.rs");
}

/// A signet BOLT11 invoice, from the API's webhook test example.
const EXAMPLE_INVOICE: &str = "lntbs10n1p4ft36app509h8j7cecenwaktcsrshu6cx4xsczarwfj4nl4tfjq6y7rz79vmqdqqcqzzsxqrrsssp5grs2zd2al99uw93vja4mc5wus9lshcjteuqwef2xkhlp9ppmkens9qxpqysgqmn4s544p8p0263cv83s3c3qjv69z3xxy3fh64ny2y766mfnqfxrrhjj8s7h0lrkxux23dfyzxfydvgjhrsw2p8z0nw54en5a9rvk4kgqxkh73g";

/// A documented placeholder as a value the CLI accepts: IDs become a fixed UUID, an invoice
/// a signet invoice, and an address a BIP 173 testnet address. Described changes never send
/// them.
fn placeholder(word: &str) -> &str {
    match word {
        "BOLT11_INVOICE" => EXAMPLE_INVOICE,
        "BITCOIN_ADDRESS" => "tb1qrp33g0q5c5txsp9arysrx4k6zdkfs4nce4xj0gdcccefvpysxf3q0sl5k7",
        id if id.ends_with("_ID") && id.chars().all(|c| c.is_ascii_uppercase() || c == '_') => {
            RESOURCE
        }
        word => word,
    }
}

/// Request bodies for the files the documented and help `--data` examples name, just
/// complete enough for local validation. A new `@FILE` in an example needs a body here.
fn data_stubs() -> [(&'static str, Value); 8] {
    [
        (
            "payment.json",
            json!({"id": RESOURCE, "wallet_id": WALLET, "currency": "btc", "type": "bolt11",
                   "data": {"payment_request": EXAMPLE_INVOICE}}),
        ),
        ("policies.json", json!({})),
        (
            "test-delivery.json",
            json!({"delivery_id": RESOURCE, "payload": {"type": "test",
                   "detail": {"event": "created", "data": "Checking the endpoint"}}}),
        ),
        (
            "movement.json",
            json!({"id": RESOURCE, "wallet_id": WALLET}),
        ),
        ("sandbox.json", json!({})),
        (
            "session-request.json",
            json!({"wallet_id": WALLET, "id": RESOURCE}),
        ),
        ("stream.json", json!({})),
        ("settings.json", json!({})),
    ]
}

/// The names in a directory, to show that a run created nothing in it.
fn file_names(dir: &Path) -> BTreeSet<OsString> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect()
}

/// Run each example that sends a change without `--execute` instead, and require that it is
/// described (exit 6) without a request reaching the API: flags, units, scope, and `--data`
/// bodies still validate as documented, and no file is created.
async fn assert_changes_describe_offline(examples: Vec<String>, minimum: usize) {
    let (server, dir) = fixture().await;
    let mut settings = seeded_api_key(dir.path(), "key", ACCOUNT_KEY);
    for name in ["prod", "staging", "test"] {
        settings.config.profiles.insert(
            name.into(),
            Profile {
                organization_id: uuid(ORG),
                environment_id: uuid(ENV),
                account: "key".into(),
            },
        );
    }
    settings.save().unwrap();
    let stubs = data_stubs();
    for (name, body) in &stubs {
        std::fs::write(dir.path().join(name), body.to_string()).unwrap();
    }
    // `--data -` reads stdin, which the docs fill with `cat payment.json |`.
    let stdin = stubs[0].1.to_string();
    let before = file_names(dir.path());
    let changes: Vec<String> = examples
        .into_iter()
        .filter(|example| example.split_whitespace().any(|word| word == "--execute"))
        .collect();
    assert!(changes.len() >= minimum, "{changes:?}");
    for example in &changes {
        let args = example
            .split_whitespace()
            .skip(1)
            .filter(|word| *word != "--execute")
            .map(placeholder);
        // Examples without a profile leave the scope to a picker; the variables stand in.
        let mut command = cli(dir.path(), &server);
        command
            .current_dir(dir.path())
            .env_remove("VOLTAGE_EXECUTE")
            .env("VOLTAGE_ORGANIZATION_ID", ORG)
            .env("VOLTAGE_ENVIRONMENT_ID", ENV)
            .env("VOLTAGE_WALLET_ID", WALLET)
            .args(args);
        if example.contains("--data -") {
            command.write_stdin(stdin.clone());
        }
        let result = command.assert();
        let output = result.get_output();
        assert_eq!(
            output.status.code(),
            Some(6),
            "{example}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let body: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["outcome"], "dry_run", "{example}");
    }
    // No `--output-file`, journal, or other file appears.
    assert_eq!(file_names(dir.path()), before);
    assert!(server.received_requests().await.unwrap().is_empty());
}

/// The lines of the section of `help` under `heading`, up to the next blank line.
fn help_section<'a>(help: &'a str, heading: &str) -> Vec<&'a str> {
    help.split_once(&format!("\n{heading}\n"))
        .map(|(_, section)| {
            section
                .lines()
                .take_while(|line| !line.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Every example in each command's `--help`, found by walking the command lists the binary
/// prints, as a person or an agent reading help would.
fn help_examples(dir: &Path) -> Vec<String> {
    let mut examples = Vec::new();
    let mut pending = vec![Vec::<String>::new()];
    while let Some(path) = pending.pop() {
        let output = human_process_at(dir, "http://127.0.0.1:9")
            .args(&path)
            .arg("--help")
            .output()
            .unwrap();
        assert!(output.status.success(), "{path:?}");
        let help = String::from_utf8(output.stdout).unwrap();
        for line in help_section(&help, "Commands:") {
            let name = line.split_whitespace().next().unwrap_or_default();
            if name != "help" {
                pending.push(path.iter().cloned().chain([name.to_owned()]).collect());
            }
        }
        examples.extend(
            help_section(&help, "Examples:")
                .into_iter()
                .map(str::trim)
                .filter(|line| line.starts_with("voltage "))
                .map(|line| {
                    line.split(['<', '>', '|'])
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_owned()
                }),
        );
    }
    examples
}

#[tokio::test(flavor = "multi_thread")]
async fn documented_changes_describe_themselves_offline() {
    assert_changes_describe_offline(examples::doc_examples(), 19).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn help_examples_describe_themselves_offline() {
    let dir = private_tempdir();
    assert_changes_describe_offline(help_examples(dir.path()), 15).await;
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

/// A second organization for `wallets list --all-orgs`.
const OTHER_ORG: &str = "55555555-5555-4555-8555-555555555555";

/// A login that sees `ORG` (Acme) and `OTHER_ORG` (Beta), with an exchange for each.
async fn two_organizations(auth: &MockServer) {
    let organizations = json!({"organizations": [
        {"id": ORG, "name": "Acme"}, {"id": OTHER_ORG, "name": "Beta"}
    ]});
    respond_once(auth, route("GET", "/users/current"), ok(organizations)).await;
    organization_exchange(auth, LOGIN_ACCESS, ORG, "acme-access", 1).await;
    organization_exchange(auth, LOGIN_ACCESS, OTHER_ORG, "beta-access", 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wallets_list_all_orgs_merges_every_organization_the_login_sees() {
    let auth = MockServer::start().await;
    let (api, dir) = fixture().await;
    seeded_login(dir.path(), &auth, false);
    two_organizations(&auth).await;
    respond_once(
        &api,
        route("GET", wallets_path(ORG))
            .and(header("authorization", "Bearer acme-access"))
            .and(query_param("inactive", "true")),
        ok(json!([{"id": WALLET, "name": "ops", "network": "signet"}])),
    )
    .await;
    respond_once(
        &api,
        route("GET", wallets_path(OTHER_ORG))
            .and(header("authorization", "Bearer beta-access"))
            .and(query_param("inactive", "true")),
        ok(json!([{"id": RESOURCE, "name": "treasury", "network": "mainnet"}])),
    )
    .await;

    // The command's filters apply in each organization, and each wallet names its own.
    let result = cli(dir.path(), &api)
        .args([
            "wallets",
            "list",
            "--account",
            LOGIN_NAME,
            "--all-orgs",
            "--inactive",
            "true",
        ])
        .assert()
        .success();
    let envelope: Value = serde_json::from_str(&stdout(&result)).unwrap();
    assert_eq!(
        envelope["data"],
        json!([
            {"id": WALLET, "name": "ops", "network": "signet",
             "organization_id": ORG, "organization_name": "Acme"},
            {"id": RESOURCE, "name": "treasury", "network": "mainnet",
             "organization_id": OTHER_ORG, "organization_name": "Beta"}
        ])
    );
    assert_eq!(envelope["outcome"], "retrieved");
    auth.verify().await;
    api.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wallets_list_all_orgs_shows_an_organization_column_for_people() {
    let auth = MockServer::start().await;
    let (api, dir) = fixture().await;
    seeded_login(dir.path(), &auth, false);
    two_organizations(&auth).await;
    for org in [ORG, OTHER_ORG] {
        let id = if org == ORG { WALLET } else { RESOURCE };
        respond_once(
            &api,
            route("GET", wallets_path(org)),
            ok(json!([{"id": id, "name": "w"}])),
        )
        .await;
    }
    let result = Command::from_std(human_process_at(dir.path(), &api.uri()))
        .args([
            "wallets",
            "list",
            "--account",
            LOGIN_NAME,
            "--all-orgs",
            "--output",
            "table",
        ])
        .assert()
        .success();
    let shown = stdout(&result);
    let header = shown.lines().next().unwrap();
    assert!(header.contains("ORGANIZATION NAME"), "{shown}");
    assert!(shown.contains("Acme") && shown.contains("Beta"), "{shown}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wallets_list_all_orgs_lists_the_others_then_fails_for_an_organization_that_fails() {
    let auth = MockServer::start().await;
    let (api, dir) = fixture().await;
    seeded_login(dir.path(), &auth, false);
    two_organizations(&auth).await;
    respond_once(
        &api,
        route("GET", wallets_path(ORG)),
        ok(json!([{"id": WALLET, "name": "ops"}])),
    )
    .await;
    respond_once(
        &api,
        route("GET", wallets_path(OTHER_ORG)),
        ResponseTemplate::new(403),
    )
    .await;
    let args = ["wallets", "list", "--account", LOGIN_NAME, "--all-orgs"];

    // The listed wallets are written, and the exit code is the failure's (403 is 3).
    let result = cli(dir.path(), &api).args(args).assert().code(3);
    let envelope: Value = serde_json::from_str(&stdout(&result)).unwrap();
    assert_eq!(envelope["data"].as_array().unwrap().len(), 1);
    assert_eq!(envelope["data"][0]["organization_name"], "Acme");
    let error = json_error(&result);
    assert_eq!(
        error["error"]["message"],
        format!("Could not list wallets in 1 of 2 organizations: Beta ({OTHER_ORG})")
    );
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .starts_with("The other organizations' wallets are listed.")
    );
    let failed = &error["error"]["detail"]["failed_organizations"];
    assert_eq!(failed.as_array().unwrap().len(), 1);
    assert_eq!(failed[0]["organization_id"], OTHER_ORG);
    assert_eq!(failed[0]["name"], "Beta");
    assert!(failed[0]["message"].as_str().is_some(), "{failed}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wallets_list_all_orgs_writes_nothing_when_every_organization_fails() {
    let auth = MockServer::start().await;
    let (api, dir) = fixture().await;
    seeded_login(dir.path(), &auth, false);
    two_organizations(&auth).await;
    // Two different failures: each organization is named.
    respond_once(
        &api,
        route("GET", wallets_path(ORG)),
        ResponseTemplate::new(500),
    )
    .await;
    respond_once(
        &api,
        route("GET", wallets_path(OTHER_ORG)),
        ResponseTemplate::new(404),
    )
    .await;
    let result = cli(dir.path(), &api)
        .args(["wallets", "list", "--account", LOGIN_NAME, "--all-orgs"])
        .assert()
        .code(1);
    assert!(stdout(&result).is_empty());
    let error = json_error(&result);
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Could not list wallets in 2 of 2 organizations"),
        "{error}"
    );
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("voltage auth status")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wallets_list_all_orgs_reports_a_revoked_login_as_itself() {
    let auth = MockServer::start().await;
    let (api, dir) = fixture().await;
    seeded_login(dir.path(), &auth, false);
    let organizations = json!({"organizations": [
        {"id": ORG, "name": "Acme"}, {"id": OTHER_ORG, "name": "Beta"}
    ]});
    respond_once(&auth, route("GET", "/users/current"), ok(organizations)).await;
    // The access token still lists organizations, but every exchange is refused.
    respond(
        &auth,
        route("POST", "/oauth/token"),
        ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})),
        2,
    )
    .await;
    let result = cli(dir.path(), &api)
        .args(["wallets", "list", "--account", LOGIN_NAME, "--all-orgs"])
        .assert()
        .code(3);
    assert!(stdout(&result).is_empty());
    let error = json_error(&result);
    assert_eq!(
        error["error"]["message"],
        "The saved login for person has expired or was revoked"
    );
    assert_eq!(
        error["error"]["detail"]["credential_problem"],
        "login_expired"
    );
    assert_eq!(
        error["error"]["hint"],
        format!(
            "Sign in again with `voltage login --account person --auth-url {}`.",
            auth.uri()
        )
    );
    assert!(api.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wallets_list_all_orgs_with_no_organizations_is_an_empty_list() {
    let auth = MockServer::start().await;
    let (api, dir) = fixture().await;
    seeded_login(dir.path(), &auth, false);
    respond_once(
        &auth,
        route("GET", "/users/current"),
        ok(json!({"organizations": []})),
    )
    .await;
    let result = cli(dir.path(), &api)
        .args(["wallets", "list", "--account", LOGIN_NAME, "--all-orgs"])
        .assert()
        .success();
    let envelope: Value = serde_json::from_str(&stdout(&result)).unwrap();
    assert_eq!(envelope["data"], json!([]));
    assert!(api.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn wallets_list_all_orgs_refuses_an_api_key_and_a_named_organization() {
    let (api, dir) = fixture().await;
    // An API key belongs to one organization.
    let result = cli(dir.path(), &api)
        .args(["wallets", "list", "--all-orgs"])
        .assert()
        .code(3);
    let error = json_error(&result);
    assert_eq!(
        error["error"]["message"],
        "--all-orgs needs a browser login, but the selected credential is an API key"
    );
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("--org ID")
    );
    // --org or --env names one organization, which --all-orgs contradicts.
    for scope in ["--org", "--env"] {
        let result = cli(dir.path(), &api)
            .args(["wallets", "list", "--all-orgs", scope, ORG])
            .assert()
            .code(2);
        assert!(
            json_error(&result)["error"]["message"]
                .as_str()
                .unwrap()
                .contains("cannot be used with"),
            "{scope}"
        );
    }
    assert!(api.received_requests().await.unwrap().is_empty());
}

/// `voltage login` on a terminal against `server` as both auth service and API, whose user
/// sees `organizations`, each with the environment `ENV` (prod). The profile offer lists
/// the organizations with the same request as the login's identity.
#[cfg(unix)]
async fn login_on_a_terminal(
    dir: &Path,
    organizations: Value,
    environments: Value,
    steps: &[(&'static str, &'static [u8])],
) -> (TerminalRun, MockServer) {
    login_on_a_terminal_with(dir, organizations, ok(environments), steps, |_| {}).await
}

#[cfg(unix)]
async fn login_on_a_terminal_with(
    dir: &Path,
    organizations: Value,
    environments: ResponseTemplate,
    steps: &[(&'static str, &'static [u8])],
    meanwhile: impl FnMut(usize),
) -> (TerminalRun, MockServer) {
    let server = MockServer::start().await;
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
        route("POST", "/oauth/token").and(body_string_contains("device_code=device-secret")),
        ok(json!({
            "access_token":"login-access-secret","refresh_token":"login-refresh-secret",
            "token_type":"Bearer","expires_in":600
        })),
    )
    .await;
    route("GET", "/users/current")
        .respond_with(ok(
            json!({"id": RESOURCE, "email": LOGIN_EMAIL, "organizations": organizations}),
        ))
        .mount(&server)
        .await;
    for org in [ORG, OTHER_ORG] {
        route("POST", "/oauth/token")
            .and(body_string_contains(format!("audience={org}")))
            .respond_with(ok(json!({
                "access_token": "picker-access",
                "issued_token_type": "urn:ietf:params:oauth:token-type:access_token",
                "token_type": "Bearer", "expires_in": 600, "scope": "read write"
            })))
            .mount(&server)
            .await;
        route("GET", format!("/organizations/{org}/environments"))
            .respond_with(environments.clone())
            .mount(&server)
            .await;
    }
    let uri = server.uri();
    let args = [
        "login",
        "--auth-url",
        &uri,
        "--no-browser",
        "--account",
        LOGIN_NAME,
        "--credential-store",
        "file",
        "--output",
        "table",
    ];
    let run = on_a_terminal_with(dir, &server, &args, steps, meanwhile).await;
    (run, server)
}

#[cfg(unix)]
fn saved_profiles(dir: &Path) -> std::collections::BTreeMap<String, Profile> {
    let settings = Settings::open(dir.to_path_buf()).unwrap();
    settings.config.profiles
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_on_a_terminal_saves_a_profile_pinned_to_the_only_organization() {
    let dir = private_tempdir();
    let (run, _server) = login_on_a_terminal(
        dir.path(),
        json!([{"id": ORG, "name": "Acme"}]),
        json!([{"id": ENV, "name": "prod"}]),
        &[("Profile name:", b"prod\r")],
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    for shown in [
        "Save a profile for this login, so later commands need only --profile NAME.",
        &format!("Organization: Acme  {ORG} (the only one)"),
        &format!("Environment: prod  {ENV} (the only one)"),
        "Saved profile prod. Use it with --profile prod.",
    ] {
        assert!(run.shown.contains(shown), "{shown}: {}", run.shown);
    }
    let profiles = saved_profiles(dir.path());
    let profile = &profiles["prod"];
    assert_eq!(
        (
            profile.organization_id,
            profile.environment_id,
            profile.account.as_str()
        ),
        (uuid(ORG), uuid(ENV), LOGIN_NAME)
    );
    assert!(run.stdout.contains("prod"), "{}", run.stdout);
    assert_eq!(run.modes.0, run.modes.1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_on_a_terminal_asks_between_organizations_and_esc_saves_no_profile() {
    let dir = private_tempdir();
    let (run, _server) = login_on_a_terminal(
        dir.path(),
        json!([{"id": ORG, "name": "Acme"}, {"id": OTHER_ORG, "name": "Beta"}]),
        json!([{"id": ENV, "name": "prod"}]),
        &[("Organization:", b"\r"), ("Profile name:", b"\x1b")],
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    assert!(
        run.shown.contains(&format!(
            "No profile was saved. Create one later with `voltage profiles create NAME --account {LOGIN_NAME}`."
        )),
        "{}",
        run.shown
    );
    assert!(saved_profiles(dir.path()).is_empty());
    // The login itself is saved.
    assert!(
        Settings::open(dir.path().to_path_buf())
            .unwrap()
            .config
            .accounts
            .contains_key(LOGIN_NAME)
    );
    assert_eq!(run.modes.0, run.modes.1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_on_a_terminal_reports_why_no_profile_was_saved_and_still_succeeds() {
    // A profile of that name already belongs to another login.
    let dir = private_tempdir();
    let mut settings = settings_at(dir.path());
    settings.config.profiles.insert(
        "default".into(),
        Profile {
            organization_id: uuid(ORG),
            environment_id: uuid(ENV),
            account: "someone-else".into(),
        },
    );
    settings.save().unwrap();
    let (run, _server) = login_on_a_terminal(
        dir.path(),
        json!([{"id": ORG, "name": "Acme"}]),
        json!([{"id": ENV, "name": "prod"}]),
        &[("Profile name:", b"\r")],
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    assert!(
        run.shown
            .contains("No profile was saved: Profile default already exists. Create one later"),
        "{}",
        run.shown
    );
    assert_eq!(
        saved_profiles(dir.path())["default"].account,
        "someone-else"
    );

    // A blank name is refused the same way.
    let dir = private_tempdir();
    let (run, _server) = login_on_a_terminal(
        dir.path(),
        json!([{"id": ORG, "name": "Acme"}]),
        json!([{"id": ENV, "name": "prod"}]),
        &[("Profile name:", b"  \r")],
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    assert!(
        run.shown
            .contains("No profile was saved: A profile name cannot be empty."),
        "{}",
        run.shown
    );

    // Ctrl-C at the name ends the command as an interruption, though the login is saved.
    let dir = private_tempdir();
    let (run, _server) = login_on_a_terminal(
        dir.path(),
        json!([{"id": ORG, "name": "Acme"}]),
        json!([{"id": ENV, "name": "prod"}]),
        &[("Profile name:", b"\x03")],
    )
    .await;
    assert_eq!(run.code, Some(130), "{}", run.shown);
    for shown in [
        format!("Interrupted while saving a profile; the login for {LOGIN_NAME} is saved"),
        format!("Create one later with `voltage profiles create NAME --account {LOGIN_NAME}`."),
    ] {
        assert!(run.shown.contains(&shown), "{shown}: {}", run.shown);
    }
    assert!(!run.shown.contains("No profile was saved"), "{}", run.shown);
    assert!(saved_profiles(dir.path()).is_empty());
    assert_eq!(run.modes.0, run.modes.1);

    // Ctrl-C while the offer lists environments is a signal, not a key at a prompt, and
    // reports the saved login the same way.
    let dir = private_tempdir();
    let (run, _server) = login_on_a_terminal_with(
        dir.path(),
        json!([{"id": ORG, "name": "Acme"}]),
        ok(json!([{"id": ENV, "name": "prod"}])).set_delay(Duration::from_secs(10)),
        &[("(the only one)", b"")],
        |_| {},
    )
    .await;
    assert_eq!(run.code, Some(130), "{}", run.shown);
    for shown in [
        format!("Interrupted while saving a profile; the login for {LOGIN_NAME} is saved"),
        format!("Create one later with `voltage profiles create NAME --account {LOGIN_NAME}`."),
    ] {
        assert!(run.shown.contains(&shown), "{shown}: {}", run.shown);
    }
    assert!(saved_profiles(dir.path()).is_empty());
    assert!(
        Settings::open(dir.path().to_path_buf())
            .unwrap()
            .config
            .accounts
            .contains_key(LOGIN_NAME)
    );

    // No environment to pin: nothing to save.
    let dir = private_tempdir();
    let (run, _server) = login_on_a_terminal(
        dir.path(),
        json!([{"id": ORG, "name": "Acme"}]),
        json!([]),
        &[],
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    assert!(
        run.shown.contains("No environments to choose from."),
        "{}",
        run.shown
    );
    assert!(run.shown.contains("No profile was saved."), "{}", run.shown);
    assert!(saved_profiles(dir.path()).is_empty());
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn login_on_a_terminal_offers_no_profile_when_one_already_uses_the_login() {
    let dir = private_tempdir();
    let mut settings = settings_at(dir.path());
    settings.config.profiles.insert(
        "prod".into(),
        Profile {
            organization_id: uuid(ORG),
            environment_id: uuid(ENV),
            account: LOGIN_NAME.into(),
        },
    );
    settings.save().unwrap();
    let (run, server) = login_on_a_terminal(
        dir.path(),
        json!([{"id": ORG, "name": "Acme"}]),
        json!([{"id": ENV, "name": "prod"}]),
        &[],
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    assert!(!run.shown.contains("Save a profile"), "{}", run.shown);
    // Only the login's own identity request; nothing is listed for a profile.
    let requests = server.received_requests().await.unwrap();
    let listed = requests
        .iter()
        .filter(|request| request.url.path() == "/users/current")
        .count();
    assert_eq!(listed, 1);
    assert!(
        requests
            .iter()
            .all(|request| !request.url.path().ends_with("/environments"))
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_only_wallet_is_still_asked_for_because_a_payment_spends_from_it() {
    let (server, dir) = fixture().await;
    respond_once(
        &server,
        route("GET", wallets_path(ORG)).and(query_param("environment_id", ENV)),
        ok(json!([{"id": WALLET, "name": "ops", "balances": [{"currency": "btc"}]}])),
    )
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
    let run = on_a_terminal(dir.path(), &server, &send, &[("Wallet:", b"\r")]).await;
    assert_eq!(run.code, Some(6), "{}", run.shown);
    assert!(!run.shown.contains("(the only one)"), "{}", run.shown);
    assert!(run.stdout.contains(WALLET), "{}", run.stdout);
    assert_eq!(run.modes.0, run.modes.1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_profile_saved_after_login_keeps_changes_made_while_its_prompt_was_open() {
    let other = |account: &str| Profile {
        organization_id: uuid(ORG),
        environment_id: uuid(ENV),
        account: account.into(),
    };
    // While the name prompt is open, another command saves a profile.
    let dir = private_tempdir();
    let path = dir.path().to_path_buf();
    let (run, _server) = login_on_a_terminal_with(
        dir.path(),
        json!([{"id": ORG, "name": "Acme"}]),
        ok(json!([{"id": ENV, "name": "prod"}])),
        &[("Profile name:", b"prod\r")],
        |_| {
            let mut settings = Settings::open(path.clone()).unwrap();
            settings
                .config
                .profiles
                .insert("ci".into(), other("ci-key"));
            settings.save().unwrap();
        },
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    let profiles = saved_profiles(dir.path());
    assert_eq!(profiles["ci"].account, "ci-key");
    assert_eq!(profiles["prod"].account, LOGIN_NAME);
    // The login, saved before the prompt, is kept as well.
    assert!(
        Settings::open(dir.path().to_path_buf())
            .unwrap()
            .config
            .accounts
            .contains_key(LOGIN_NAME)
    );

    // A name another command takes while the prompt is open is refused, and theirs is kept.
    let dir = private_tempdir();
    let path = dir.path().to_path_buf();
    let (run, _server) = login_on_a_terminal_with(
        dir.path(),
        json!([{"id": ORG, "name": "Acme"}]),
        ok(json!([{"id": ENV, "name": "prod"}])),
        &[("Profile name:", b"prod\r")],
        |_| {
            let mut settings = Settings::open(path.clone()).unwrap();
            settings
                .config
                .profiles
                .insert("prod".into(), other("ci-key"));
            settings.save().unwrap();
        },
    )
    .await;
    assert_eq!(run.code, Some(0), "{}", run.shown);
    assert!(
        run.shown
            .contains("No profile was saved: Profile prod already exists."),
        "{}",
        run.shown
    );
    assert_eq!(saved_profiles(dir.path())["prod"].account, "ci-key");
}

#[tokio::test]
async fn profiles_create_refuses_a_name_already_saved() {
    let (server, dir) = fixture().await;
    seeded_api_key(dir.path(), "saved", ACCOUNT_KEY);
    let create = || {
        cli(dir.path(), &server)
            .args([
                "profiles",
                "create",
                "prod",
                "--account",
                "saved",
                "--org",
                ORG,
                "--env",
                ENV,
            ])
            .assert()
    };
    create().success();
    let error = json_error(&create().code(2));
    assert_eq!(error["error"]["message"], "Profile prod already exists");
}
