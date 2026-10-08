//! `--dry-run`: describe the request an API command would send, then stop.
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
    output::{Envelope, Outcome},
    registry::{AuthScheme, Method},
};
use serde::Serialize;
use serde_json::Value;

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

/// The `data` of a dry-run envelope.
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
        notes.push("The id was generated for this dry run; a real run generates a new one unless you pass --id.");
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
    use serde_json::json;

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
