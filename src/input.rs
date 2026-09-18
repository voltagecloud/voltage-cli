use crate::{
    Error, Result, cli,
    config::{Scope, validate_uuid},
    registry::{Operation, query_flag},
};
use clap::ArgMatches;
use serde_json::{Value, json};
use std::io::Read;

pub fn amount(value: &str, unit: &str) -> Result<Value> {
    let (currency, scale) = match unit {
        "msats" => ("btc", 0),
        "sats" => ("btc", 3),
        "btc" => ("btc", 11),
        "cents" => ("usd", 0),
        "usd" => ("usd", 2),
        _ => return Err(Error::usage("Unit must be msats, sats, btc, cents, or usd")),
    };
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() > 2
        || parts[0].is_empty()
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()))
    {
        return Err(Error::usage(
            "Amount must be a nonnegative decimal without exponent notation",
        ));
    }
    let fraction = parts.get(1).copied().unwrap_or("");
    let significant = fraction.trim_end_matches('0');
    if significant.len() > scale {
        return Err(Error::usage(
            "Amount has precision smaller than the currency's base unit",
        ));
    }
    let factor = 10u128.pow(scale as u32);
    let whole = parts[0]
        .parse::<u128>()
        .map_err(|_| Error::usage("Amount overflow"))?;
    let frac = if significant.is_empty() {
        0
    } else {
        significant
            .parse::<u128>()
            .map_err(|_| Error::usage("Amount overflow"))?
            .checked_mul(10u128.pow((scale - significant.len()) as u32))
            .ok_or_else(|| Error::usage("Amount overflow"))?
    };
    let base = whole
        .checked_mul(factor)
        .and_then(|v| v.checked_add(frac))
        .filter(|v| *v <= i64::MAX as u128)
        .ok_or_else(|| Error::usage("Amount exceeds the API's int64 range"))?;
    Ok(json!({"currency":currency,"amount":base as i64}))
}
fn required(m: &ArgMatches, name: &str) -> Result<String> {
    cli::value(m, name)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            Error::usage(format!(
                "--{name} is required (or supply the complete --data payload)"
            ))
        })
}
fn uuid_field(m: &ArgMatches, name: &str) -> Result<String> {
    let v = required(m, name)?;
    validate_uuid(&v)?;
    Ok(v)
}
fn metadata(m: &ArgMatches) -> Result<Value> {
    let mut map = serde_json::Map::new();
    for pair in cli::values(m, "metadata") {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| Error::usage("Metadata must use KEY=VALUE"))?;
        if map.insert(k.into(), Value::String(v.into())).is_some() {
            return Err(Error::usage("Duplicate metadata key"));
        }
    }
    Ok(Value::Object(map))
}
fn events(m: &ArgMatches) -> Result<Value> {
    let mut events = Vec::new();
    for event in cli::values(m, "event") {
        let (kind, name) = event.split_once('.').ok_or_else(|| {
            Error::usage("Events must use kind.event, for example receive.completed")
        })?;
        events.push(json!({kind: name}));
    }
    if events.is_empty() {
        return Err(Error::usage("At least one --event is required"));
    }
    Ok(Value::Array(events))
}

pub fn body(
    op: &Operation,
    path: &[String],
    m: &ArgMatches,
    scope: &Scope,
) -> Result<Option<Value>> {
    if !op.body {
        return Ok(None);
    }
    let mut body = if let Some(source) = cli::value(m, "data") {
        let reader: Box<dyn Read> = if source == "-" {
            Box::new(std::io::stdin())
        } else if let Some(file) = source.strip_prefix('@') {
            Box::new(std::fs::File::open(file)?)
        } else {
            return Err(Error::usage("Use --data @file or --data -"));
        };
        let mut bytes = Vec::new();
        reader.take(16 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(Error::usage("Request body exceeds 16 MiB"));
        }
        let v: Value = serde_json::from_slice(&bytes)?;
        if !v.is_object() {
            return Err(Error::usage("Request body must be a JSON object"));
        }
        v
    } else {
        match op.id.as_str() {
            "create_wallet" => {
                json!({"id":new_id(m)?,"environment_id":single_env(scope)?,"line_of_credit_id":uuid_field(m,"credit-line")?,"name":required(m,"name")?,"network":required(m,"network")?,"limit":required(m,"limit")?.parse::<u64>().map_err(|_|Error::usage("--limit must be an unsigned integer in the credit line's base units"))?,"metadata":metadata(m)?})
            }
            "update_wallet" => json!({"name":required(m,"name")?}),
            "create_payment" => {
                let wallet = scope
                    .wallet
                    .as_ref()
                    .ok_or_else(|| Error::usage("--wallet is required for payment creation"))?;
                let mut b = json!({"id":new_id(m)?,"wallet_id":wallet});
                let currency = required(m, "currency")?;
                if currency != "btc" && currency != "usd" {
                    return Err(Error::usage(
                        "Convenience commands support btc and usd; use --data for other currencies",
                    ));
                }
                let receive = path.last().is_some_and(|p| p == "receive");
                if receive {
                    let kind = required(m, "kind")?;
                    if !["bolt11", "onchain", "bip21"].contains(&kind.as_str()) {
                        return Err(Error::usage(
                            "Receive --kind must be bolt11, onchain, or bip21",
                        ));
                    }
                    if cli::value(m, "invoice").is_some()
                        || cli::value(m, "address").is_some()
                        || cli::value(m, "max-fee").is_some()
                    {
                        return Err(Error::usage(
                            "Receive commands do not accept send destination or fee flags",
                        ));
                    }
                    b["payment_kind"] = json!(kind);
                    if let Some(v) = cli::value(m, "amount") {
                        let a = amount(&v, &required(m, "unit")?)?;
                        if a["currency"] != currency {
                            return Err(Error::usage(
                                "Receive currency must match the amount unit",
                            ));
                        }
                        b["amount"] = a;
                    } else {
                        b["currency"] = json!(currency);
                    }
                    if let Some(v) = cli::value(m, "expiration") {
                        b["expiration"] = json!(
                            v.parse::<u64>()
                                .map_err(|_| Error::usage("Invalid expiration"))?
                        );
                    }
                    if let Some(v) = cli::value(m, "description") {
                        b["description"] = json!(v);
                    }
                } else {
                    let invoice = cli::value(m, "invoice");
                    let address = cli::value(m, "address");
                    if invoice.is_some() == address.is_some() {
                        return Err(Error::usage(
                            "Provide exactly one of --invoice or --address; use payments receive for receiving",
                        ));
                    }
                    if cli::value(m, "kind").is_some() || cli::value(m, "expiration").is_some() {
                        return Err(Error::usage("Send commands do not accept receive flags"));
                    }
                    let mut data = if let Some(invoice) = invoice {
                        b["type"] = json!("bolt11");
                        json!({"payment_request":invoice})
                    } else {
                        b["type"] = json!("onchain");
                        json!({"address":address.unwrap()})
                    };
                    b["currency"] = json!(currency);
                    if let Some(v) = cli::value(m, "amount") {
                        let a = amount(&v, &required(m, "unit")?)?;
                        if a["amount"].as_i64() == Some(0) {
                            return Err(Error::usage("Send amount must be greater than zero"));
                        }
                        data["amount"] = a;
                    }
                    if b["type"] == "onchain" && data.get("amount").is_none() {
                        return Err(Error::usage("On-chain sends require --amount and --unit"));
                    }
                    if let Some(v) = cli::value(m, "max-fee") {
                        let fee = amount(&v, &required(m, "fee-unit")?)?;
                        if fee["currency"] != "btc" {
                            return Err(Error::usage("Network fees must use BTC units"));
                        }
                        data["max_fee"] = fee;
                    }
                    if let Some(v) = cli::value(m, "description") {
                        if b["type"] == "bolt11" {
                            return Err(Error::usage("BOLT11 sends do not accept --description"));
                        }
                        data["description"] = json!(v);
                    }
                    b["data"] = data;
                }
                if let Some(v) = cli::value(m, "quote") {
                    validate_uuid(&v)?;
                    b["quote_id"] = json!(v);
                }
                let meta = metadata(m)?;
                if !meta.as_object().unwrap().is_empty() {
                    b["metadata"] = meta;
                }
                if cli::value(m, "unit").is_some() && cli::value(m, "amount").is_none() {
                    return Err(Error::usage("--unit requires --amount"));
                }
                if cli::value(m, "fee-unit").is_some() && cli::value(m, "max-fee").is_none() {
                    return Err(Error::usage("--fee-unit requires --max-fee"));
                }
                b
            }
            "request_a_quote" => {
                json!({"id":new_id(m)?,"line_of_credit_id":uuid_field(m,"credit-line")?,"network":required(m,"network")?,"amount":amount(&required(m,"amount")?,&required(m,"unit")?)?,"to":required(m,"to")?})
            }
            "create_webhook" => {
                json!({"id":new_id(m)?,"name":required(m,"name")?,"url":required(m,"url")?,"events":events(m)?})
            }
            "update_webhook" => json!({"events":events(m)?}),
            _ => {
                return Err(Error::usage(
                    "This operation requires --data @file or --data -",
                ));
            }
        }
    };
    if let Some(id) = body.get("id").and_then(Value::as_str) {
        validate_uuid(id)?;
    }
    if [
        "create_payment",
        "create_treasury_movement",
        "check_payment",
        "create_session",
    ]
    .contains(&op.id.as_str())
    {
        let wallet = body["wallet_id"]
            .as_str()
            .ok_or_else(|| Error::usage("The complete JSON payload must include wallet_id"))?;
        validate_uuid(wallet)?;
        if scope.wallet.as_ref().is_some_and(|w| w != wallet) {
            return Err(Error::usage("--wallet conflicts with payload wallet_id"));
        }
    }
    if op.id == "create_wallet" {
        let env = body["environment_id"]
            .as_str()
            .ok_or_else(|| Error::usage("Wallet payload requires environment_id"))?;
        validate_uuid(env)?;
        if !scope.envs.is_empty() && (scope.envs.len() != 1 || scope.envs[0] != env) {
            return Err(Error::usage(
                "Environment scope conflicts with payload environment_id",
            ));
        }
    }
    if op.id == "create_payment" || op.id == "create_treasury_movement" || op.id == "check_payment"
    {
        if body.get("id").and_then(Value::as_str).is_none() {
            return Err(Error::usage(
                "The complete payment payload requires an id; generate one once and retain it for reconciliation",
            ));
        }
        let usd = body["currency"] == "usd" || body["amount"]["currency"] == "usd";
        if usd && body.get("quote_id").is_none() {
            return Err(Error::usage(
                "USD payment flows require an explicit quote_id",
            ));
        }
        if path.last().is_some_and(|p| p == "send") && body.get("payment_kind").is_some() {
            return Err(Error::usage(
                "payments send cannot submit a receive payload",
            ));
        }
        if path.last().is_some_and(|p| p == "receive") && body.get("payment_kind").is_none() {
            return Err(Error::usage("payments receive requires a receive payload"));
        }
    }
    // Preserve JSON, including unknown extension fields and exact numeric values.
    Ok(Some(body.take()))
}
fn new_id(m: &ArgMatches) -> Result<String> {
    let id = cli::value(m, "id").unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_uuid(&id)?;
    Ok(id)
}
pub fn single_env(scope: &Scope) -> Result<&str> {
    if scope.envs.len() != 1 {
        return Err(Error::usage(
            "Exactly one --env is required for this operation",
        ));
    }
    Ok(&scope.envs[0])
}

pub fn path(op: &Operation, m: &ArgMatches, scope: &Scope) -> Result<String> {
    let mut path = op.path.clone();
    for p in op.parameters.iter().filter(|p| p.location == "path") {
        let v = if op.target.as_ref() == Some(&p.name) {
            cli::value(m, "resource-id")
        } else {
            match p.name.as_str() {
                "organization_id" => scope.org.clone(),
                "environment_id" => Some(single_env(scope)?.into()),
                "wallet_id" => scope.wallet.clone(),
                "webhook_id" => scope.webhook.clone(),
                _ => None,
            }
        }
        .ok_or_else(|| Error::usage(format!("Missing {} for {}", p.name, op.command.join(" "))))?;
        validate_uuid(&v)?;
        path = path.replace(&format!("{{{}}}", p.name), &v);
    }
    Ok(path)
}

pub fn query(op: &Operation, m: &ArgMatches, scope: &Scope) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for p in op
        .parameters
        .iter()
        .filter(|p| p.location == "query" && p.name != "stream_token")
    {
        let values = match p.name.as_str() {
            "environment_id" | "environment_ids" => scope.envs.clone(),
            "wallet_id" => scope.wallet.clone().into_iter().collect(),
            _ => cli::values(m, &query_flag(&p.name)),
        };
        if p.name == "environment_id" && values.len() > 1 {
            return Err(Error::usage(
                "This operation accepts only one environment filter",
            ));
        }
        for v in values {
            serialize_query(p, &v, &mut pairs)?;
        }
    }
    for pair in cli::values(m, "query") {
        let (name, value) = pair
            .split_once('=')
            .ok_or_else(|| Error::usage("--query requires NAME=VALUE"))?;
        let root = name.split('[').next().unwrap_or(name);
        let param = op
            .parameters
            .iter()
            .find(|p| p.location == "query" && p.name == root && p.name != "stream_token")
            .ok_or_else(|| Error::usage(format!("Unknown query parameter {name}")))?;
        if ["environment_id", "environment_ids", "wallet_id"].contains(&root) {
            return Err(Error::usage(
                "Use --env or --wallet for scope filters so profile conflicts can be checked",
            ));
        }
        if name.contains('[') {
            pairs.push((name.into(), value.into()));
        } else {
            serialize_query(param, value, &mut pairs)?;
        }
    }
    if op.parameters.iter().any(|p| p.name == "pagination")
        && !pairs
            .iter()
            .any(|(k, _)| k == "pagination" || k == "offset")
    {
        pairs.push(("pagination".into(), "cursor".into()));
    }
    Ok(pairs)
}
fn serialize_query(
    p: &crate::registry::Parameter,
    value: &str,
    out: &mut Vec<(String, String)>,
) -> Result<()> {
    let object = p.schema["type"] == "object"
        || p.style.as_deref() == Some("deepObject") && p.name == "metadata";
    let array = p.schema["type"] == "array"
        || p.schema.get("items").is_some()
        || p.name == "statuses"
        || p.name == "environment_ids";
    if object {
        let (k, v) = value
            .split_once('=')
            .ok_or_else(|| Error::usage("Metadata filters require KEY=VALUE"))?;
        out.push((format!("{}[{k}]", p.name), v.into()));
    } else if array {
        // The API uses bracket arrays even where its OpenAPI style is omitted.
        out.push((format!("{}[]", p.name), value.into()));
    } else {
        out.push((p.name.clone(), value.into()));
    }
    Ok(())
}

pub fn consequential(op: &Operation, body: Option<&Value>) -> bool {
    op.method == "DELETE"
        || ["create_treasury_movement", "generate_webhook_key"].contains(&op.id.as_str())
        || op.id == "create_payment" && !body.is_some_and(is_receive)
        || op.id == "update_sandbox_line_of_credit" && body.is_some_and(|b| b["disable"] == true)
}

fn is_receive(body: &Value) -> bool {
    // A send discriminator or send data always requires confirmation, including
    // mixed raw payloads. A nullable or unknown receive kind cannot exempt a send.
    body.get("type").is_none()
        && body.get("data").is_none()
        && body["payment_kind"]
            .as_str()
            .is_some_and(|kind| ["bolt11", "onchain", "bip21", "taprootasset"].contains(&kind))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_money() {
        assert_eq!(amount("0.00000001", "btc").unwrap()["amount"], 1000);
        assert_eq!(amount("12.34", "usd").unwrap()["amount"], 1234);
        assert!(amount("0.0001", "sats").is_err());
        assert!(amount("9223372036854775808", "msats").is_err());
        assert_eq!(
            amount("9223372036854775807", "msats").unwrap()["amount"].as_i64(),
            Some(i64::MAX)
        );
        assert!(amount("-1", "btc").is_err());
    }
    #[test]
    fn raw_sends_require_confirmation() {
        let op = crate::registry::operation("create_payment");
        assert!(consequential(op, Some(&json!({"type":"bolt11"}))));
        assert!(!consequential(op, Some(&json!({"payment_kind":"bolt11"}))));
    }
}
