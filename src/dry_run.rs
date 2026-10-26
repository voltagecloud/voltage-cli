//! A change without `--execute`: describe the request it would send, then stop.
//!
//! The description is built after local validation and before authentication, wallet
//! preflight, confirmation, journaling, or any network request. It reports the `Run` that
//! `api::execute` would follow, so it cannot drift from a real run. Body values are omitted
//! unless their field is known to carry no private data; `--show-secrets` shows the body.

use crate::{
    Result,
    api::{OFFSET_PAGINATION_DEPRECATED, Run},
    auth,
    cli::{ApiInvocation, BodySource, GlobalFlags},
    config::API_URL,
    input::{PaginationMode, Request},
    output::{self, Envelope, Outcome},
    payment::PaymentDirection,
    registry::{AuthScheme, Method},
    style::{Line, Role},
    terminal::Terminal,
};
use serde::Serialize;
use serde_json::{Map, Value};

/// Body fields whose values are identifiers, enums, or amounts. Everything else, including
/// invoices, addresses, descriptions, names, URLs, and metadata, is omitted by default.
const SHOWN_FIELDS: [&str; 16] = [
    "id",
    "wallet_id",
    "environment_id",
    "line_of_credit_id",
    "quote_id",
    "currency",
    "amount",
    "max_fee",
    "type",
    "payment_kind",
    "network",
    "limit",
    "to",
    "expiration",
    "disable",
    "data",
];

const SEND_DATA: &str = "data";
/// Webhook event selections, shown when they have the contract's shape: one-key objects
/// whose value is an event-name enum, such as `{"receive": "completed"}`.
const EVENTS: &str = "events";
const OMITTED: &str = "[OMITTED]";

#[derive(Serialize)]
struct QueryPair<'a> {
    name: &'a str,
    value: &'a str,
}

/// The `data` of a described change's envelope (`outcome: dry_run`).
#[derive(Serialize)]
struct DryRun<'a> {
    operation: String,
    method: Method,
    url: String,
    query: Vec<QueryPair<'a>>,
    /// The `Origin` header from `--origin`, which a checkout session may require.
    origin: Option<&'a str>,
    body: Option<Value>,
    /// Private body and query values were replaced with `[OMITTED]`.
    values_omitted: bool,
    authentication: AuthScheme,
    /// What a real run would do after authenticating; none of it was done.
    run: Run,
    notes: Vec<&'static str>,
}

/// Describe the request without sending it.
pub fn describe(
    invocation: &ApiInvocation,
    global: &GlobalFlags,
    request: &Request,
    run: Run,
) -> Result<Envelope> {
    let operation = invocation.operation;
    let show = global.show_secrets;
    let base = auth::base_url(global.api_url.as_deref().unwrap_or(API_URL))?;
    let query = request
        .query
        .iter()
        .map(|(name, value)| QueryPair {
            name,
            value: if show || !name.starts_with("metadata[") {
                value
            } else {
                OMITTED
            },
        })
        .collect::<Vec<_>>();
    let body = request.body.clone().map(|mut body| {
        if !show {
            omit_private(&mut body);
        }
        body
    });
    let values_omitted = query.iter().any(|pair| pair.value == OMITTED)
        || body.as_ref().is_some_and(contains_omission);
    let mut notes =
        vec!["No request was sent. Local validation does not prove the API will accept it."];
    if generated_id(invocation) {
        notes.push("The id was generated for this description; a real run generates a new one unless you pass --id.");
    }
    if matches!(invocation.body, Some(BodySource::Raw(_))) {
        notes.push("The --data payload was read and validated; a real run reads it again.");
    }
    if operation.id.returns_one_time_secret() {
        notes.push("This operation returns a one-time secret; a real run requires --output-file PATH or --show-secrets.");
    }
    if request.pagination_mode() == PaginationMode::Offset && operation.has_parameter("cursor") {
        notes.push(OFFSET_PAGINATION_DEPRECATED);
    }
    Ok(Envelope::new(
        None,
        serde_json::to_value(DryRun {
            operation: invocation.command_name(),
            method: operation.method,
            url: format!("{base}{}", request.path),
            query,
            origin: invocation.origin.as_ref().map(|origin| origin.as_str()),
            body,
            values_omitted,
            authentication: operation.auth,
            run,
            notes,
        })?,
        request.resource_id,
        Outcome::DryRun,
    ))
}

/// On a terminal, say in words what the described change would do and the values it would
/// use, before the description's full details. It shows only what the description shows.
pub fn announce(terminal: Terminal, invocation: &ApiInvocation, described: &Envelope) {
    announce_change(terminal, &action(invocation), &facts(invocation, described));
}

/// `announce` for any change: what it would do, such as "create an environment", and the
/// values it would use.
pub fn announce_change(terminal: Terminal, action: &str, facts: &Value) {
    let caution = Line::default()
        .paint(Role::Caution, "!")
        .text(" Not sent: this would ")
        .styled(Role::Emphasis, action);
    terminal.notice_line(caution);
    for line in output::field_lines(facts) {
        terminal.notice_line(Line::default().text(&format!("  {line}")));
    }
    terminal.notice_line(Line::default());
}

/// The change in words: the payment direction for `payments send` and `receive`, else the
/// operation's summary, such as "delete a wallet".
fn action(invocation: &ApiInvocation) -> String {
    match invocation.alias {
        Some(PaymentDirection::Send) => "send a payment".into(),
        Some(PaymentDirection::Receive) => "receive a payment".into(),
        None => {
            let summary = invocation.operation.description.trim_end_matches('.');
            let summary = summary.strip_suffix(" as a user").unwrap_or(summary);
            let mut chars = summary.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_lowercase().chain(chars).collect()
            })
        }
    }
}

/// The target resource and the body values the description shows. Omitted values and an
/// `id` generated only for the description are left out, and a send's `data` fields read as
/// the payment's own.
fn facts(invocation: &ApiInvocation, described: &Envelope) -> Value {
    let mut facts = Map::new();
    // The request path's target, which a raw `--data` body's own `id` does not change.
    if let (Some(target), Some(id)) = (invocation.operation.target, invocation.resource_id) {
        facts.insert(
            target.parameter_name().into(),
            Value::String(id.to_string()),
        );
    }
    let Some(Value::Object(body)) = described.data.get("body").and_then(shown) else {
        return Value::Object(facts);
    };
    for (key, value) in body {
        match value {
            _ if key == "id" && generated_id(invocation) => {}
            Value::Object(data) if key == SEND_DATA => {
                for (key, value) in data {
                    facts.entry(key).or_insert(value);
                }
            }
            value => {
                facts.entry(key).or_insert(value);
            }
        }
    }
    Value::Object(facts)
}

/// `value` without its omitted parts, or `None` when nothing is left.
fn shown(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) if text == OMITTED => None,
        Value::Object(map) => {
            let kept = map
                .iter()
                .filter_map(|(key, value)| Some((key.clone(), shown(value)?)))
                .collect::<Map<_, _>>();
            (!kept.is_empty()).then_some(Value::Object(kept))
        }
        Value::Null => None,
        other => Some(other.clone()),
    }
}

/// A friendly create built its own `id` because `--id` was not given.
fn generated_id(invocation: &ApiInvocation) -> bool {
    match &invocation.body {
        Some(BodySource::Friendly(flags)) => flags.generates_id(),
        Some(BodySource::Raw(_)) | None => false,
    }
}

/// Replace every value outside `SHOWN_FIELDS` with `[OMITTED]`, recursively. A send's
/// `data` is shown only as an object, so its own fields are filtered too.
fn omit_private(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if key == EVENTS && is_event_selection(value) {
                    continue;
                }
                let shown =
                    SHOWN_FIELDS.contains(&key.as_str()) && (key != SEND_DATA || value.is_object());
                if shown {
                    omit_private(value);
                } else {
                    *value = Value::String(OMITTED.into());
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(omit_private),
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {}
    }
}

fn is_event_selection(value: &Value) -> bool {
    value.as_array().is_some_and(|events| {
        events.iter().all(|event| {
            event
                .as_object()
                .is_some_and(|event| event.len() == 1 && event.values().all(Value::is_string))
        })
    })
}

fn contains_omission(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.values().any(contains_omission),
        Value::Array(values) => values.iter().any(contains_omission),
        Value::String(text) => text == OMITTED,
        Value::Number(_) | Value::Bool(_) | Value::Null => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Command, Invocation};
    use serde_json::json;
    use uuid::Uuid;

    const WALLET: &str = "33333333-3333-4333-8333-333333333333";

    fn api(args: &[&str]) -> ApiInvocation {
        let mut full = vec!["voltage"];
        full.extend_from_slice(args);
        match Invocation::try_parse_from(full).unwrap().command {
            Command::Api(api) => *api,
            Command::Local(local) => panic!("expected an API command, got {local:?}"),
        }
    }

    fn described(body: Value, resource_id: Option<Uuid>) -> Envelope {
        Envelope::new(None, json!({"body": body}), resource_id, Outcome::DryRun)
    }

    #[test]
    fn a_change_reads_as_its_payment_direction_or_its_summary() {
        let send = api(&["payments", "send", "--wallet", WALLET, "--invoice", "lnbc1"]);
        assert_eq!(action(&send), "send a payment");
        let receive = api(&[
            "payments",
            "receive",
            "--kind",
            "bolt11",
            "--currency",
            "btc",
        ]);
        assert_eq!(action(&receive), "receive a payment");
        assert_eq!(
            action(&api(&["wallets", "delete", WALLET])),
            "delete a wallet"
        );
        // A trailing period and "as a user" are dropped from the API's summary.
        assert_eq!(
            action(&api(&["quotes", "create", "--wallet", WALLET])),
            "request a currency conversion quote"
        );
        assert_eq!(
            action(&api(&[
                "wallets", "policies", "update", WALLET, "--data", "@p.json"
            ])),
            "update a wallet's policies"
        );
    }

    #[test]
    fn facts_name_the_target_and_the_shown_values_only() {
        let delete = api(&["wallets", "delete", WALLET]);
        let wallet = Uuid::parse_str(WALLET).unwrap();
        assert_eq!(
            facts(&delete, &described(Value::Null, Some(wallet))),
            json!({"wallet_id": WALLET})
        );
        // A raw body's `id` can differ from the argument; the target is the one the path uses.
        let other = "22222222-2222-4222-8222-222222222222";
        let update = api(&["wallets", "update", WALLET, "--data", "@b.json"]);
        let body = json!({"id": other, "name": "x"});
        let other_id = Uuid::parse_str(other).unwrap();
        assert_eq!(
            facts(&update, &described(body, Some(other_id)))["wallet_id"],
            json!(WALLET)
        );
        // A send's `data` fields are its own; omitted values and the generated ID are left out.
        let send = api(&["payments", "send", "--wallet", WALLET, "--invoice", "lnbc1"]);
        let body = json!({
            "id": "generated", "wallet_id": WALLET, "type": "bolt11", "currency": "btc",
            "data": {"payment_request": OMITTED, "amount": {"currency": "btc", "amount": 1000}},
            "metadata": OMITTED, "note": null
        });
        assert_eq!(
            facts(&send, &described(body, None)),
            json!({"wallet_id": WALLET, "type": "bolt11", "currency": "btc",
                "amount": {"currency": "btc", "amount": 1000}})
        );
        // An ID the person gave stays.
        let chosen = api(&[
            "payments",
            "send",
            "--wallet",
            WALLET,
            "--invoice",
            "lnbc1",
            "--id",
            "44444444-4444-4444-8444-444444444444",
        ]);
        assert_eq!(
            facts(
                &chosen,
                &described(json!({"id": "chosen", "data": OMITTED}), None)
            ),
            json!({"id": "chosen"})
        );
        assert_eq!(facts(&send, &described(json!(OMITTED), None)), json!({}));
    }

    #[test]
    fn private_body_values_are_omitted_and_identifiers_and_amounts_kept() {
        let mut body = json!({
            "id": "p", "wallet_id": "w", "type": "bolt11", "currency": "btc",
            "data": {
                "payment_request": "lnbc1secret", "address": "bc1q",
                "amount": {"currency": "btc", "amount": 1000},
                "max_fee": {"currency": "btc", "amount": 10},
                "description": "private"
            },
            "metadata": {"order": "42"},
            "events": [{"receive": "completed"}],
            "url": "https://hooks.example.test/?token=t"
        });
        let mut scalar_data = json!({"data": "private"});
        omit_private(&mut scalar_data);
        assert_eq!(scalar_data, json!({"data": OMITTED}));
        omit_private(&mut body);
        assert_eq!(
            body,
            json!({
                "id": "p", "wallet_id": "w", "type": "bolt11", "currency": "btc",
                "data": {
                    "payment_request": OMITTED, "address": OMITTED,
                    "amount": {"currency": "btc", "amount": 1000},
                    "max_fee": {"currency": "btc", "amount": 10},
                    "description": OMITTED
                },
                "metadata": OMITTED,
                "events": [{"receive": "completed"}],
                "url": OMITTED
            })
        );
        assert!(contains_omission(&body));
        // Anything but the contract's event shape is treated like any other private value.
        let mut nested = json!({"events": [{"receive": {"secret": "s"}}]});
        omit_private(&mut nested);
        assert_eq!(nested, json!({"events": OMITTED}));
        assert!(!contains_omission(&json!({"id": "p", "amount": [1]})));
    }
}
