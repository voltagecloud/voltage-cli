use assert_cmd::Command;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use voltage_cli::{
    config::{self, Account, Config, Credential, Settings},
    registry::OPERATIONS,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, header, method, path},
};

const ORG: &str = "11111111-1111-4111-8111-111111111111";
const ENV: &str = "22222222-2222-4222-8222-222222222222";
const WALLET: &str = "33333333-3333-4333-8333-333333333333";
const RESOURCE: &str = "44444444-4444-4444-8444-444444444444";

fn private_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

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
    cmd.env("VOLTAGE_API_KEY", "test-account-key")
        .arg("--config-dir")
        .arg(dir)
        .arg("--api-url")
        .arg(api_url)
        .arg("--json");
    cmd
}
fn json_stdout(assertion: &assert_cmd::assert::Assert) -> Value {
    serde_json::from_slice(&assertion.get_output().stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_documented_operation_reaches_its_exact_route_and_auth_scheme() {
    for op in OPERATIONS.iter() {
        let server = MockServer::start().await;
        let dir = private_tempdir();
        let target = if op.target.as_deref() == Some("wallet_id") {
            WALLET
        } else {
            RESOURCE
        };
        let route = op
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
        let body = if op.id == "create_wallet" {
            json!({"id":RESOURCE,"environment_id":ENV,"line_of_credit_id":RESOURCE,"name":"wallet","network":"mutinynet","limit":0})
        } else {
            json!({"id":RESOURCE,"wallet_id":WALLET,"currency":"btc","type":"bolt11","data":{"payment_request":"example","amount":{"currency":"btc","amount":9223372036854775807i64}},"extension":{"preserved":true}})
        };
        let response = if op.id == "get_events" {
            ResponseTemplate::new(200).set_body_raw(
                "event: updated\ndata: {\"status\":\"completed\"}\n\n",
                "text/event-stream",
            )
        } else if op.method == "GET" {
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":target,"environment_id":ENV,"extra":"preserved"}))
        } else {
            ResponseTemplate::new(202)
        };
        let mut mock = Mock::given(method(op.method.as_str())).and(path(route.clone()));
        if op.auth == "account" {
            mock = mock.and(header("x-api-key", "test-account-key"));
        }
        if op.auth == "checkout_session" {
            mock = mock.and(header("authorization", "Bearer checkout-key"));
        }
        if op.body {
            mock = mock.and(body_json(body.clone()));
        }
        mock.respond_with(response).expect(1).mount(&server).await;
        if op.target.as_deref() == Some("wallet_id") && op.method != "GET" {
            Mock::given(method("GET"))
                .and(path(route.split("/policies").next().unwrap()))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"environment_id":ENV})),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        let mut cmd = cli(dir.path(), &server);
        cmd.args(&op.command)
            .args(["--org", ORG, "--env", ENV, "--yes", "--show-secrets"]);
        if op.target.is_some() {
            cmd.arg(target);
        }
        if op.path.contains("{webhook_id}") && op.target.as_deref() != Some("webhook_id") {
            cmd.args(["--webhook", RESOURCE]);
        }
        if op.body {
            let request = dir.path().join("request.json");
            std::fs::write(&request, body.to_string()).unwrap();
            cmd.args(["--data", &format!("@{}", request.display())]);
        }
        if op.auth == "checkout_session" {
            cmd.env("VOLTAGE_CHECKOUT_TOKEN", "checkout-key");
        }
        if op.auth == "checkout_stream" {
            cmd.env("VOLTAGE_STREAM_TOKEN", "stream-key");
        }
        let mut expected_query = Vec::new();
        for p in op
            .parameters
            .iter()
            .filter(|p| p.location == "query" && p.name != "stream_token")
        {
            let value = match p.name.as_str() {
                "environment_id" | "environment_ids" => ENV,
                "wallet_id" => WALLET,
                "metadata" => "order=a&b=3",
                "inactive"
                | "include_inactive"
                | "include_disabled"
                | "exclude_zero_amount_bills" => "true",
                "limit" => "10",
                "offset" => "0",
                "start_date" | "end_date" => "2026-09-09T10:30:00Z",
                "pagination" => "cursor",
                "network" => "mutinynet",
                "statuses" => "completed",
                "sort_order" => "asc",
                "sort_key" => "created_at",
                "payment_category" => "lightning",
                "kind" => "bolt11",
                "direction" => "send",
                _ => RESOURCE,
            };
            if p.name == "wallet_id" {
                cmd.args(["--wallet", value]);
            } else if !p.name.starts_with("environment_id") {
                cmd.args([
                    &format!("--{}", voltage_cli::registry::query_flag(&p.name)),
                    value,
                ]);
            }
            let pair = if p.name == "metadata" {
                ("metadata[order]".to_owned(), "a&b=3".to_owned())
            } else {
                let name = if p.schema.get("items").is_some() {
                    format!("{}[]", p.name)
                } else {
                    p.name.clone()
                };
                (name, value.to_owned())
            };
            expected_query.push(pair);
        }
        let result = cmd.assert().success();
        let output = json_stdout(&result);
        if op.method != "GET" {
            assert_eq!(output["outcome"], "accepted", "{}", op.id);
        }
        let requests = server.received_requests().await.unwrap();
        let request = requests
            .iter()
            .find(|r| r.method.as_str() == op.method && r.url.path() == route)
            .unwrap();
        for (name, value) in expected_query {
            assert!(
                request
                    .url
                    .query_pairs()
                    .any(|(k, v)| k == name && v == value),
                "{} omitted query parameter {}",
                op.id,
                name
            );
        }
        if op.auth != "account" {
            assert!(
                request.headers.get("x-api-key").is_none(),
                "{} leaked account credential",
                op.id
            );
        }
        if op.auth == "none" || op.auth == "checkout_stream" {
            assert!(request.headers.get("authorization").is_none());
        }
        if op.auth == "checkout_stream" {
            assert_eq!(
                request
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "stream_token")
                    .unwrap()
                    .1,
                "stream-key"
            );
        }
        server.verify().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_send_without_yes_never_submits() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
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
        assert!(String::from_utf8_lossy(&result.get_output().stderr).contains("requires --yes"));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(!dir.path().join("requests").exists());
}
#[tokio::test(flavor = "multi_thread")]
async fn secret_destination_is_required_before_request() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    cli(dir.path(), &server)
        .args([
            "webhooks", "keys", "rotate", RESOURCE, "--org", ORG, "--env", ENV, "--yes",
        ])
        .assert()
        .code(2);
    assert!(server.received_requests().await.unwrap().is_empty());
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
        let message = String::from_utf8_lossy(&result.get_output().stderr);
        assert!(message.contains("HTTP read"), "{message}");
        assert!(!message.contains("write"), "{message}");
        assert!(!message.contains("mutation"), "{message}");
        assert!(!message.contains("test-account-key"));
        assert!(!message.contains("private-response-token"));
        peer.join().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn payment_timeout_preserves_id_and_does_not_retry() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202).set_delay(Duration::from_secs(3)))
        .expect(1)
        .mount(&server)
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
    assert!(String::from_utf8_lossy(&result.get_output().stderr).contains(RESOURCE));
    assert!(
        String::from_utf8_lossy(&result.get_output().stderr)
            .contains("a write may have been submitted. No mutation was retried.")
    );
    assert!(
        dir.path()
            .join(format!("requests/{RESOURCE}.json"))
            .exists()
    );
    server.verify().await;
}
#[tokio::test(flavor = "multi_thread")]
async fn profile_credentials_override_ambient_key_and_scope() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    let mut settings = Settings {
        dir: dir.path().into(),
        config: Config::default(),
    };
    settings.config.accounts.insert(
        "stage".into(),
        Account {
            store: "file".into(),
            kind: "api_key".into(),
            email: None,
            organization_id: Some(ORG.into()),
            environment_id: Some(ENV.into()),
            auth_url: config::AUTH_URL.into(),
        },
    );
    let mut credential = Credential::default();
    credential.api_key = Some("stage-key".into());
    settings
        .write_credential("stage", "file", &credential)
        .unwrap();
    settings.config.profiles.insert(
        "stage".into(),
        config::Profile {
            organization_id: ORG.into(),
            environment_id: ENV.into(),
            account: "stage".into(),
        },
    );
    settings.save().unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/organizations/{ORG}/wallets")))
        .and(header("x-api-key", "stage-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&server)
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
async fn checkout_session_retries_projection_and_never_uses_account_key() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = count.clone();
    Mock::given(method("GET"))
        .respond_with(move |_: &wiremock::Request| {
            if counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                ResponseTemplate::new(202)
                    .insert_header("x-retry-after-ms", "1")
                    .set_body_json(json!({"id":RESOURCE}))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"id":RESOURCE,"status":"ready"}))
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    cli(dir.path(), &server)
        .env("VOLTAGE_CHECKOUT_TOKEN", "checkout-key")
        .args(["checkout", "sessions", "get", RESOURCE])
        .assert()
        .success();
    for r in server.received_requests().await.unwrap() {
        assert!(r.headers.get("x-api-key").is_none());
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn query_encoding_preserves_arrays_and_metadata() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"items":[],"has_more":false})),
        )
        .expect(1)
        .mount(&server)
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
async fn file_output_retains_one_time_secret_with_private_permissions() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    let out = dir.path().join("secret.json");
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_json(json!({"id":RESOURCE,"shared_secret":"one-time"})),
        )
        .expect(1)
        .mount(&server)
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
    let server = MockServer::start().await;
    let dir = private_tempdir();
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(json!({"message":"Bad key test-account-key"})),
        )
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG])
        .assert()
        .code(3);
    assert!(!String::from_utf8_lossy(&result.get_output().stderr).contains("test-account-key"));
}

fn saved_user(dir: &Path, server: &MockServer, expired: bool) -> Settings {
    let mut settings = Settings {
        dir: dir.into(),
        config: Config::default(),
    };
    settings.config.accounts.insert(
        "person".into(),
        Account {
            store: "file".into(),
            kind: "user".into(),
            email: Some("person@example.test".into()),
            organization_id: None,
            environment_id: None,
            auth_url: server.uri(),
        },
    );
    settings
        .write_credential(
            "person",
            "file",
            &Credential {
                access_token: Some("old-access".into()),
                refresh_token: Some("old-refresh".into()),
                expires_at: Some(if expired {
                    0
                } else {
                    voltage_cli::auth::now() + 600
                }),
                user_id: Some(RESOURCE.into()),
                email: Some("person@example.test".into()),
                api_key: None,
            },
        )
        .unwrap();
    settings.save().unwrap();
    settings
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_builders_send_documented_event_variants() {
    for action in ["create", "update"] {
        let server = MockServer::start().await;
        let dir = private_tempdir();
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
        Mock::given(method(if action == "create" { "POST" } else { "PATCH" }))
            .and(body_json(payload))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
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
        let server = MockServer::start().await;
        let dir = private_tempdir();
        Mock::given(method("POST"))
            .and(body_json(json!({"id":RESOURCE,"environment_id":ENV,"line_of_credit_id":RESOURCE,"name":"demo","network":network,"limit":0,"metadata":{}})))
            .respond_with(ResponseTemplate::new(202)).expect(1).mount(&server).await;
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
    let server = MockServer::start().await;
    let dir = private_tempdir();
    for network in ["bitcoin", "testnet", "signet", "regtest"] {
        cli(dir.path(), &server)
            .args(["wallets", "create", "--network", network])
            .assert()
            .code(2);
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_for_a_payment_tolerates_an_initial_missing_projection() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let server = MockServer::start().await;
    let dir = private_tempdir();
    let count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .respond_with(move |_: &wiremock::Request| {
            if count.fetch_add(1, Ordering::SeqCst) < 2 {
                ResponseTemplate::new(404).set_body_json(json!({"error":"not_found"}))
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":RESOURCE,"status":"completed"}))
            }
        })
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
    let server = MockServer::start().await;
    let dir = private_tempdir();
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
    assert!(String::from_utf8_lossy(&result.get_output().stderr).contains(RESOURCE));
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
async fn concurrent_processes_refresh_once_and_save_the_rotated_token() {
    use wiremock::matchers::body_string_contains;
    let server = MockServer::start().await;
    let dir = private_tempdir();
    let settings = saved_user(dir.path(), &server, true);
    Mock::given(method("POST")).and(path("/oauth/token")).and(body_string_contains("refresh_token=old-refresh")).respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(100)).set_body_json(json!({"access_token":"new-access","refresh_token":"new-refresh","token_type":"Bearer","expires_in":600}))).expect(1).mount(&server).await;
    organization_exchange(&server, "new-access", ORG, "org-access", 2).await;
    Mock::given(method("GET"))
        .and(path(format!("/organizations/{ORG}/wallets")))
        .and(header("authorization", "Bearer org-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(2)
        .mount(&server)
        .await;
    let command = || {
        let mut cmd = cli(dir.path(), &server);
        cmd.args(["wallets", "list", "--account", "person", "--org", ORG]);
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
        settings
            .read_credential("person")
            .unwrap()
            .access_token
            .as_deref(),
        Some("new-access")
    );
    assert_eq!(
        settings
            .read_credential("person")
            .unwrap()
            .refresh_token
            .as_deref(),
        Some("new-refresh")
    );
}

async fn organization_exchange(
    server: &MockServer,
    login: &str,
    org: &str,
    access: &str,
    count: u64,
) {
    use wiremock::matchers::body_string_contains;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange",
        ))
        .and(body_string_contains(
            "subject_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aaccess_token",
        ))
        .and(body_string_contains(format!("subject_token={login}")))
        .and(body_string_contains(format!("audience={org}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": access,
            "issued_token_type": "urn:ietf:params:oauth:token-type:access_token",
            "token_type": "Bearer", "expires_in": 600, "scope": "read write"
        })))
        .expect(count)
        .up_to_n_times(count)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn organization_exchange_uses_the_saved_auth_origin_and_keeps_discovery_on_login() {
    let auth = MockServer::start().await;
    let api = MockServer::start().await;
    let dir = private_tempdir();
    let settings = saved_user(dir.path(), &auth, false);
    Mock::given(method("GET"))
        .and(path("/users/current"))
        .and(header("authorization", "Bearer old-access"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"organizations": [{"id": ORG}]})),
        )
        .expect(1)
        .mount(&auth)
        .await;
    cli(dir.path(), &api)
        .args(["organizations", "list", "--account", "person"])
        .assert()
        .success();
    for (org, token) in [(ORG, "first-org-access"), (RESOURCE, "second-org-access")] {
        organization_exchange(&auth, "old-access", org, token, 1).await;
        Mock::given(method("GET"))
            .and(path(format!("/organizations/{org}/wallets")))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .mount(&api)
            .await;
        cli(dir.path(), &api)
            .args(["wallets", "list", "--account", "person", "--org", org])
            .assert()
            .success();
    }
    organization_exchange(&auth, "old-access", ORG, "environment-access", 1).await;
    Mock::given(method("GET"))
        .and(path(format!("/organizations/{ORG}/environments")))
        .and(header("authorization", "Bearer environment-access"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .expect(1)
        .mount(&auth)
        .await;
    cli(dir.path(), &api)
        .args(["environments", "list", "--account", "person", "--org", ORG])
        .assert()
        .success();
    let saved = settings.read_credential("person").unwrap();
    assert_eq!(saved.access_token.as_deref(), Some("old-access"));
    assert_eq!(saved.refresh_token.as_deref(), Some("old-refresh"));
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
        let api = MockServer::start().await;
        let dir = private_tempdir();
        let settings = saved_user(dir.path(), &auth, false);
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .expect(1)
            .mount(&auth)
            .await;
        let result = cli(dir.path(), &api)
            .args(["wallets", "list", "--account", "person", "--org", ORG])
            .assert()
            .code(3);
        let output = String::from_utf8_lossy(&result.get_output().stdout);
        assert!(
            !output.contains("old-access")
                && !output.contains("old-refresh")
                && !output.contains("bad-access")
        );
        assert!(api.received_requests().await.unwrap().is_empty());
        assert_eq!(
            settings
                .read_credential("person")
                .unwrap()
                .access_token
                .as_deref(),
            Some("old-access")
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn logout_retains_credentials_on_revocation_failure_and_local_is_explicit() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    let settings = saved_user(dir.path(), &server, false);
    Mock::given(method("POST"))
        .and(path("/oauth/revoke"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error":"unavailable"})))
        .expect(1)
        .mount(&server)
        .await;
    cli(dir.path(), &server)
        .args(["logout", "--account", "person"])
        .assert()
        .code(3);
    assert!(settings.read_credential("person").is_ok());
    cli(dir.path(), &server)
        .args(["logout", "--account", "person", "--local"])
        .assert()
        .success();
    assert!(settings.read_credential("person").is_err());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn browser_device_login_handles_pending_and_slowdown_without_exposing_tokens() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let server = MockServer::start().await;
    let dir = private_tempdir();
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    Mock::given(method("POST")).and(path("/oauth/device_authorization")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"device_code":"device-secret","user_code":"ABCD-EFGH","verification_uri":"https://app.voltage.cloud/cli/authorize","expires_in":60,"interval":5}))).expect(1).mount(&server).await;
    Mock::given(method("POST")).and(path("/oauth/token")).respond_with(move |_:&wiremock::Request|match count.fetch_add(1,Ordering::SeqCst) {
        0=>ResponseTemplate::new(400).set_body_json(json!({"error":"authorization_pending"})),
        1=>ResponseTemplate::new(429).set_body_string("Too Many Requests"),
        _=>ResponseTemplate::new(200).set_body_json(json!({"access_token":"login-access-secret","refresh_token":"login-refresh-secret","token_type":"Bearer","expires_in":600}))
    }).expect(3).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/users/current"))
        .and(header("authorization", "Bearer login-access-secret"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":RESOURCE,"email":"person@example.test"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let begin = std::time::Instant::now();
    let a = cli(dir.path(), &server)
        .args([
            "login",
            "--auth-url",
            &server.uri(),
            "--no-browser",
            "--account",
            "person",
            "--credential-store",
            "file",
        ])
        .assert()
        .success();
    assert!(begin.elapsed() >= Duration::from_secs(20));
    assert_eq!(json_stdout(&a)["data"]["authenticated"], true);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&a.get_output().stdout),
        String::from_utf8_lossy(&a.get_output().stderr)
    );
    for secret in [
        "device-secret",
        "login-access-secret",
        "login-refresh-secret",
    ] {
        assert!(!combined.contains(secret));
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn qr_implies_invoice_readiness_wait() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    Mock::given(method("POST"))
        .and(path(format!(
            "/organizations/{ORG}/environments/{ENV}/payments"
        )))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/organizations/{ORG}/environments/{ENV}/payments/{RESOURCE}"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": RESOURCE,
            "direction": "receive",
            "type": "bolt11",
            "status": "receiving",
            "data": {"payment_request": "lntbs1example"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .env("VOLTAGE_WALLET_ID", WALLET)
        .args([
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
            "--amount",
            "1",
            "--unit",
            "sats",
            "--qr",
        ])
        .assert()
        .success();
    assert_eq!(json_stdout(&result)["outcome"], "ready");
    let stderr = String::from_utf8_lossy(&result.get_output().stderr);
    assert!(stderr.contains("Payment 44444444-4444-4444-8444-444444444444 status: receiving"));
    assert!(stderr.contains("Scan to pay:"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_tolerates_projection_delay_and_distinguishes_invoice_from_settlement() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let server = MockServer::start().await;
    let dir = private_tempdir();
    let count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path(format!(
            "/organizations/{ORG}/environments/{ENV}/payments"
        )))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path(format!("/organizations/{ORG}/environments/{ENV}/payments/{RESOURCE}"))).respond_with(move|_:&wiremock::Request|match count.fetch_add(1,Ordering::SeqCst){0=>ResponseTemplate::new(404).set_body_json(json!({"error":"not_found"})),1=>ResponseTemplate::new(200).set_body_json(json!({"id":RESOURCE,"status":"receiving","data":{"payment_request":"invoice"}})),_=>ResponseTemplate::new(200).set_body_json(json!({"id":RESOURCE,"status":"completed"}))}).expect(3).mount(&server).await;
    let a = cli(dir.path(), &server)
        .env("VOLTAGE_WALLET_ID", WALLET)
        .args([
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
            "--amount",
            "1",
            "--unit",
            "sats",
            "--wait",
            "completed",
        ])
        .assert()
        .success();
    assert_eq!(json_stdout(&a)["outcome"], "completed");
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn offset_pagination_with_has_more_preserves_filters() {
    use wiremock::matchers::query_param;
    let server = MockServer::start().await;
    let dir = private_tempdir();
    for (offset, more) in [("0", true), ("1", false)] {
        Mock::given(method("GET")).and(path(format!("/organizations/{ORG}/environments/{ENV}/payments"))).and(query_param("offset",offset)).and(query_param("pagination","offset")).and(query_param("limit","1")).respond_with(ResponseTemplate::new(200).set_body_json(json!({"items":[{"id":RESOURCE}],"offset":offset.parse::<u64>().unwrap(),"limit":1,"has_more":more,"next_cursor":null}))).expect(1).mount(&server).await;
    }
    let a = cli(dir.path(), &server)
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
        json_stdout(&a)["data"]["pages"].as_array().unwrap().len(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn denied_and_expired_device_requests_never_save_credentials() {
    for (error, exit) in [("access_denied", 3), ("expired_token", 5)] {
        let server = MockServer::start().await;
        let dir = private_tempdir();
        Mock::given(method("POST")).and(path("/oauth/device_authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"device_code":"secret-device", "user_code":"ABCD-EFGH", "verification_uri":"https://app.voltage.cloud/cli/authorize", "expires_in":60,"interval":5})))
            .expect(1).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error":error})))
            .expect(1)
            .mount(&server)
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
async fn wait_timeout_retains_the_accepted_payment_id() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id":RESOURCE,"status":"receiving"})),
        )
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args([
            "payments",
            "receive",
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
            "--kind",
            "bolt11",
            "--wait",
            "completed",
            "--timeout",
            "2",
        ])
        .assert()
        .code(5);
    assert!(String::from_utf8_lossy(&result.get_output().stderr).contains(RESOURCE));
    assert!(
        dir.path()
            .join(format!("requests/{RESOURCE}.json"))
            .exists()
    );
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ambiguous_payment_is_reconciled_by_reading_the_same_id_without_resubmission() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(202).set_delay(Duration::from_secs(3)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/organizations/{ORG}/environments/{ENV}/payments/{RESOURCE}"
        )))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"id":RESOURCE,"status":"generating"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    let result = cli(dir.path(), &server)
        .args([
            "payments",
            "receive",
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
            "--kind",
            "bolt11",
            "--timeout",
            "1",
        ])
        .assert()
        .success();
    assert_eq!(json_stdout(&result)["outcome"], "accepted");
    assert_eq!(json_stdout(&result)["resource_id"], RESOURCE);
    assert!(String::from_utf8_lossy(&result.get_output().stderr).contains("reconciled"));
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ambiguous_account_selection_and_missing_ids_fail_before_http() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    let mut settings = saved_user(dir.path(), &server, false);
    settings
        .config
        .accounts
        .insert("another".into(), settings.config.accounts["person"].clone());
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

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn insecure_file_credentials_fail_without_falling_back() {
    use std::os::unix::fs::PermissionsExt;
    let server = MockServer::start().await;
    let dir = private_tempdir();
    saved_user(dir.path(), &server, false);
    let secret = std::fs::read_dir(dir.path().join("credentials"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o644)).unwrap();
    cli(dir.path(), &server)
        .args(["wallets", "list", "--org", ORG, "--account", "person"])
        .assert()
        .code(2);
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_credential_store_failure_never_creates_plaintext_credentials() {
    let server = MockServer::start().await;
    let dir = private_tempdir();
    let mut settings = Settings {
        dir: dir.path().into(),
        config: Config::default(),
    };
    settings.config.accounts.insert(
        "missing".into(),
        Account {
            store: "keychain".into(),
            kind: "user".into(),
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
