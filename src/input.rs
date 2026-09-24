//! Request construction: path substitution, query encoding, and JSON bodies.
//!
//! Friendly flags build typed payloads. Raw `--data` bodies are preserved byte for byte,
//! including unknown extension fields and exact numbers, and only checked for the facts
//! the CLI relies on: identity, scope agreement, and whether confirmation is required.

use crate::{
    Error, Result,
    cli::{
        ApiInvocation, BodySource, Filter, FriendlyFlags, PaymentFlags, QueryOverride, QuoteFlags,
        UpdateWalletFlags, UpdateWebhookFlags, WalletFlags, WebhookFlags,
    },
    config::{InputSource, Scope},
    payment::{Amount, AmountUnit, Currency, Network, PaymentDirection, ReceiveKind},
    registry::{Method, Operation, OperationId, Parameter, ParameterLocation, ScopeParameter},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io::Read;
use uuid::Uuid;

/// Raw bodies are bounded before parsing.
const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

const PAGINATION_PARAMETER: &str = "pagination";
const OFFSET_PARAMETER: &str = "offset";

/// How a list operation pages, as its query names it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaginationMode {
    Cursor,
    Offset,
}

impl PaginationMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Cursor => "cursor",
            Self::Offset => "offset",
        }
    }
}

/// One operation's request, ready to send.
#[derive(Debug)]
pub struct Request {
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
    /// The `id` the body carries, which the recovery journal records before submission.
    pub body_id: Option<Uuid>,
    /// The resource the request names: the body's `id`, else the positional target.
    pub resource_id: Option<Uuid>,
}

impl Request {
    pub fn build(invocation: &ApiInvocation, scope: &Scope) -> Result<Self> {
        let operation = invocation.operation;
        let body = body(invocation, scope)?;
        let body_id = body.as_ref().and_then(|body| RawPayload(body).id());
        Ok(Self {
            path: path(operation, invocation.resource_id, scope)?,
            query: query(operation, &invocation.filters, &invocation.query, scope)?,
            body,
            body_id,
            resource_id: body_id.or(invocation.resource_id),
        })
    }

    /// The receive kind a payment body names, if it is one the CLI knows.
    pub fn receive_kind(&self) -> Option<ReceiveKind> {
        self.body
            .as_ref()
            .and_then(|body| RawPayload(body).receive_kind())
    }

    /// Offset paging is explicit; every other list request pages by cursor.
    pub fn pagination_mode(&self) -> PaginationMode {
        let offset = self.query.iter().any(|(name, value)| {
            name == OFFSET_PARAMETER
                || (name == PAGINATION_PARAMETER && value == PaginationMode::Offset.as_str())
        });
        if offset {
            PaginationMode::Offset
        } else {
            PaginationMode::Cursor
        }
    }

    /// The explicit offset in the query, when offset paging was requested.
    pub fn offset(&self) -> Option<u64> {
        self.query
            .iter()
            .find(|(name, _)| name == OFFSET_PARAMETER)
            .and_then(|(_, value)| value.parse().ok())
    }

    /// Replace one query parameter with the next page's value.
    pub fn advance(&mut self, name: String, value: String) {
        self.query.retain(|(existing, _)| *existing != name);
        self.query.push((name, value));
    }

    /// Sends, deletes, treasury movements, key rotation, and disabling credit lines are
    /// confirmed before submission. A receive is exempt only when the payload is
    /// unambiguously a receive.
    pub fn is_consequential(&self, operation: &Operation) -> bool {
        let payload = self.body.as_ref().map(RawPayload);
        operation.method == Method::Delete
            || matches!(
                operation.id,
                OperationId::CreateTreasuryMovement | OperationId::GenerateWebhookKey
            )
            || (operation.id == OperationId::CreatePayment
                && !payload.is_some_and(|payload| payload.is_receive()))
            || (operation.id == OperationId::UpdateSandboxLineOfCredit
                && payload.is_some_and(|payload| payload.disables()))
    }
}

/// Read view over a complete JSON payload for the facts the CLI checks.
#[derive(Clone, Copy)]
struct RawPayload<'a>(&'a Value);

impl RawPayload<'_> {
    fn text(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(Value::as_str)
    }

    fn uuid(&self, name: &str) -> Result<Option<Uuid>> {
        self.text(name).map(parse_uuid).transpose()
    }

    /// The payload's `id`, ignoring non-text values as the API would reject them.
    fn id(&self) -> Option<Uuid> {
        self.text("id").and_then(|id| id.parse().ok())
    }

    fn has(&self, name: &str) -> bool {
        self.0.get(name).is_some()
    }

    fn is_usd(&self) -> bool {
        self.0["currency"] == Currency::Usd.as_wire()
            || self.0["amount"]["currency"] == Currency::Usd.as_wire()
    }

    fn receive_kind(&self) -> Option<ReceiveKind> {
        self.0
            .get("payment_kind")
            .and_then(|kind| ReceiveKind::deserialize(kind).ok())
    }

    /// A send discriminator or send data always requires confirmation, including mixed
    /// payloads. A nullable or unknown receive kind cannot exempt a send.
    fn is_receive(&self) -> bool {
        !self.has("type") && !self.has("data") && self.receive_kind().is_some()
    }

    fn disables(&self) -> bool {
        self.0["disable"] == true
    }
}

impl Currency {
    fn as_wire(self) -> &'static str {
        match self {
            Self::Btc => "btc",
            Self::Usd => "usd",
        }
    }
}

fn parse_uuid(value: &str) -> Result<Uuid> {
    value
        .parse()
        .map_err(|_| Error::usage(format!("Expected UUID, got {value}")))
}

fn missing(flag: &str) -> Error {
    Error::usage(format!(
        "--{flag} is required (or supply the complete --data payload)"
    ))
}

fn require<T>(value: Option<T>, flag: &str) -> Result<T> {
    value.ok_or_else(|| missing(flag))
}

fn require_text(value: Option<&str>, flag: &str) -> Result<String> {
    value
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| missing(flag))
}

/// `--id` or a fresh UUID; convenience creates always send an ID for reconciliation.
fn new_id(id: Option<Uuid>) -> Uuid {
    id.unwrap_or_else(Uuid::new_v4)
}

fn metadata(pairs: &[String]) -> Result<Map<String, Value>> {
    let mut map = Map::new();
    for pair in pairs {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| Error::usage("Metadata must use KEY=VALUE"))?;
        if map
            .insert(key.into(), Value::String(value.into()))
            .is_some()
        {
            return Err(Error::usage("Duplicate metadata key"));
        }
    }
    Ok(map)
}

/// Webhook events are `kind.event` pairs encoded as one-key objects.
fn events(selection: &[String]) -> Result<Vec<Map<String, Value>>> {
    let events = selection
        .iter()
        .map(|event| {
            event
                .split_once('.')
                .map(|(kind, name)| Map::from_iter([(kind.into(), Value::String(name.into()))]))
                .ok_or_else(|| {
                    Error::usage("Events must use kind.event, for example receive.completed")
                })
        })
        .collect::<Result<Vec<_>>>()?;
    if events.is_empty() {
        return Err(Error::usage("At least one --event is required"));
    }
    Ok(events)
}

#[derive(Serialize)]
struct CreateWalletBody {
    id: Uuid,
    environment_id: Uuid,
    line_of_credit_id: Uuid,
    name: String,
    network: Network,
    limit: u64,
    metadata: Map<String, Value>,
}

#[derive(Serialize)]
struct UpdateWalletBody {
    name: String,
}

#[derive(Serialize)]
struct ReceiveBody {
    id: Uuid,
    wallet_id: Uuid,
    payment_kind: ReceiveKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    amount: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    currency: Option<Currency>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expiration: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quote_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<Map<String, Value>>,
}

/// Where a send pays to; the API names the kind in `type` and the target in `data`.
enum SendDestination {
    Bolt11 { payment_request: String },
    Onchain { address: String },
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum SendKind {
    Bolt11,
    Onchain,
}

#[derive(Serialize)]
struct SendData<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    payment_request: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    amount: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_fee: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
}

#[derive(Serialize)]
struct SendBody<'a> {
    id: Uuid,
    wallet_id: Uuid,
    #[serde(rename = "type")]
    kind: SendKind,
    currency: Currency,
    data: SendData<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quote_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<Map<String, Value>>,
}

#[derive(Serialize)]
struct QuoteBody {
    id: Uuid,
    line_of_credit_id: Uuid,
    network: Network,
    amount: Amount,
    to: Currency,
}

#[derive(Serialize)]
struct CreateWebhookBody {
    id: Uuid,
    name: String,
    url: String,
    events: Vec<Map<String, Value>>,
}

#[derive(Serialize)]
struct UpdateWebhookBody {
    events: Vec<Map<String, Value>>,
}

fn create_wallet(flags: &WalletFlags, scope: &Scope) -> Result<CreateWalletBody> {
    Ok(CreateWalletBody {
        id: new_id(flags.id),
        environment_id: scope.single_env()?,
        line_of_credit_id: require(flags.credit_line, "credit-line")?,
        name: require_text(flags.name.as_deref(), "name")?,
        network: require(flags.network, "network")?,
        limit: require(flags.limit, "limit")?,
        metadata: metadata(&flags.metadata)?,
    })
}

fn amount_with_unit(
    amount: Option<&str>,
    unit: Option<AmountUnit>,
    unit_flag: &str,
) -> Result<Option<Amount>> {
    amount
        .map(|value| Amount::parse(value, require(unit, unit_flag)?))
        .transpose()
}

fn receive(flags: &PaymentFlags, wallet: Uuid, currency: Currency) -> Result<ReceiveBody> {
    let kind = require(flags.kind, "kind")?;
    if flags.invoice.is_some() || flags.address.is_some() || flags.max_fee.is_some() {
        return Err(Error::usage(
            "Receive commands do not accept send destination or fee flags",
        ));
    }
    let amount = amount_with_unit(flags.amount.as_deref(), flags.unit, "unit")?;
    if amount.is_some_and(|amount| amount.currency != currency) {
        return Err(Error::usage("Receive currency must match the amount unit"));
    }
    let metadata = metadata(&flags.metadata)?;
    Ok(ReceiveBody {
        id: new_id(flags.id),
        wallet_id: wallet,
        payment_kind: kind,
        currency: amount.is_none().then_some(currency),
        amount,
        expiration: flags.expiration,
        description: flags.description.clone(),
        quote_id: flags.quote,
        metadata: (!metadata.is_empty()).then_some(metadata),
    })
}

fn send_body(flags: &PaymentFlags, wallet: Uuid, currency: Currency) -> Result<Value> {
    let destination = match (&flags.invoice, &flags.address) {
        (Some(invoice), None) => SendDestination::Bolt11 {
            payment_request: invoice.clone(),
        },
        (None, Some(address)) => SendDestination::Onchain {
            address: address.clone(),
        },
        _ => {
            return Err(Error::usage(
                "Provide exactly one of --invoice or --address; use payments receive for receiving",
            ));
        }
    };
    if flags.kind.is_some() || flags.expiration.is_some() {
        return Err(Error::usage("Send commands do not accept receive flags"));
    }
    let amount = amount_with_unit(flags.amount.as_deref(), flags.unit, "unit")?;
    if amount.is_some_and(|amount| amount.amount == 0) {
        return Err(Error::usage("Send amount must be greater than zero"));
    }
    if matches!(destination, SendDestination::Onchain { .. }) && amount.is_none() {
        return Err(Error::usage("On-chain sends require --amount and --unit"));
    }
    let max_fee = amount_with_unit(flags.max_fee.as_deref(), flags.fee_unit, "fee-unit")?;
    if max_fee.is_some_and(|fee| fee.currency != Currency::Btc) {
        return Err(Error::usage("Network fees must use BTC units"));
    }
    if flags.description.is_some() && matches!(destination, SendDestination::Bolt11 { .. }) {
        return Err(Error::usage("BOLT11 sends do not accept --description"));
    }
    let metadata = metadata(&flags.metadata)?;
    let (kind, payment_request, address) = match &destination {
        SendDestination::Bolt11 { payment_request } => {
            (SendKind::Bolt11, Some(payment_request.as_str()), None)
        }
        SendDestination::Onchain { address } => (SendKind::Onchain, None, Some(address.as_str())),
    };
    Ok(serde_json::to_value(SendBody {
        id: new_id(flags.id),
        wallet_id: wallet,
        kind,
        currency,
        data: SendData {
            payment_request,
            address,
            amount,
            max_fee,
            description: flags.description.as_deref(),
        },
        quote_id: flags.quote,
        metadata: (!metadata.is_empty()).then_some(metadata),
    })?)
}

fn payment(flags: &PaymentFlags, alias: Option<PaymentDirection>, scope: &Scope) -> Result<Value> {
    let wallet = scope
        .wallet
        .ok_or_else(|| Error::usage("--wallet is required for payment creation"))?;
    let currency = require(flags.currency, "currency")?;
    if flags.unit.is_some() && flags.amount.is_none() {
        return Err(Error::usage("--unit requires --amount"));
    }
    if flags.fee_unit.is_some() && flags.max_fee.is_none() {
        return Err(Error::usage("--fee-unit requires --max-fee"));
    }
    match alias {
        Some(PaymentDirection::Receive) => {
            Ok(serde_json::to_value(receive(flags, wallet, currency)?)?)
        }
        Some(PaymentDirection::Send) | None => send_body(flags, wallet, currency),
    }
}

fn quote(flags: &QuoteFlags) -> Result<QuoteBody> {
    let amount = require_text(flags.amount.as_deref(), "amount")?;
    Ok(QuoteBody {
        id: new_id(flags.id),
        line_of_credit_id: require(flags.credit_line, "credit-line")?,
        network: require(flags.network, "network")?,
        amount: Amount::parse(&amount, require(flags.unit, "unit")?)?,
        to: require(flags.to, "to")?,
    })
}

fn friendly_body(
    flags: &FriendlyFlags,
    alias: Option<PaymentDirection>,
    scope: &Scope,
) -> Result<Value> {
    Ok(match flags {
        FriendlyFlags::CreateWallet(flags) => serde_json::to_value(create_wallet(flags, scope)?)?,
        FriendlyFlags::UpdateWallet(UpdateWalletFlags { name }) => {
            serde_json::to_value(UpdateWalletBody {
                name: require_text(name.as_deref(), "name")?,
            })?
        }
        FriendlyFlags::Payment(flags) => payment(flags, alias, scope)?,
        FriendlyFlags::Quote(flags) => serde_json::to_value(quote(flags)?)?,
        FriendlyFlags::CreateWebhook(WebhookFlags {
            id,
            name,
            url,
            event,
        }) => serde_json::to_value(CreateWebhookBody {
            id: new_id(*id),
            name: require_text(name.as_deref(), "name")?,
            url: require_text(url.as_deref(), "url")?,
            events: events(event)?,
        })?,
        FriendlyFlags::UpdateWebhook(UpdateWebhookFlags { event }) => {
            serde_json::to_value(UpdateWebhookBody {
                events: events(event)?,
            })?
        }
    })
}

/// A complete JSON object from stdin or a file, bounded and otherwise untouched.
fn raw_body(source: &InputSource) -> Result<Value> {
    let reader: Box<dyn Read> = match source {
        InputSource::Stdin => Box::new(std::io::stdin()),
        InputSource::File(path) => Box::new(std::fs::File::open(path)?),
    };
    let mut bytes = Vec::new();
    reader.take(MAX_BODY_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BODY_BYTES {
        return Err(Error::usage("Request body exceeds 16 MiB"));
    }
    let body: Value = serde_json::from_slice(&bytes)?;
    if !body.is_object() {
        return Err(Error::usage("Request body must be a JSON object"));
    }
    Ok(body)
}

fn body(invocation: &ApiInvocation, scope: &Scope) -> Result<Option<Value>> {
    let operation = invocation.operation;
    let body = match &invocation.body {
        None if operation.body => {
            return Err(Error::usage(
                "This operation requires --data @file or --data -",
            ));
        }
        None => return Ok(None),
        Some(BodySource::Raw(source)) => raw_body(source)?,
        Some(BodySource::Friendly(flags)) => friendly_body(flags, invocation.alias, scope)?,
    };
    check_payload(operation, invocation.alias, RawPayload(&body), scope)?;
    Ok(Some(body))
}

/// Facts the CLI relies on before sending any body, friendly or raw.
fn check_payload(
    operation: &Operation,
    alias: Option<PaymentDirection>,
    payload: RawPayload<'_>,
    scope: &Scope,
) -> Result<()> {
    if let Some(id) = payload.text("id") {
        parse_uuid(id)?;
    }
    if operation.id.requires_wallet_in_body() {
        let wallet = payload
            .uuid("wallet_id")?
            .ok_or_else(|| Error::usage("The complete JSON payload must include wallet_id"))?;
        if scope.wallet.is_some_and(|selected| selected != wallet) {
            return Err(Error::usage("--wallet conflicts with payload wallet_id"));
        }
    }
    if operation.id == OperationId::CreateWallet {
        let env = payload
            .uuid("environment_id")?
            .ok_or_else(|| Error::usage("Wallet payload requires environment_id"))?;
        if !scope.envs.is_empty() && scope.envs != [env] {
            return Err(Error::usage(
                "Environment scope conflicts with payload environment_id",
            ));
        }
    }
    if operation.id.takes_payment_payload() {
        if payload.text("id").is_none() {
            return Err(Error::usage(
                "The complete payment payload requires an id; generate one once and retain it for reconciliation",
            ));
        }
        if payload.is_usd() && !payload.has("quote_id") {
            return Err(Error::usage(
                "USD payment flows require an explicit quote_id",
            ));
        }
        match alias {
            Some(PaymentDirection::Send) if payload.has("payment_kind") => {
                return Err(Error::usage(
                    "payments send cannot submit a receive payload",
                ));
            }
            Some(PaymentDirection::Receive) if !payload.has("payment_kind") => {
                return Err(Error::usage("payments receive requires a receive payload"));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Substitute every path placeholder from the positional target or the scope.
fn path(operation: &Operation, resource_id: Option<Uuid>, scope: &Scope) -> Result<String> {
    let mut path = operation.path.clone();
    for parameter in operation
        .parameters
        .iter()
        .filter(|parameter| parameter.location == ParameterLocation::Path)
    {
        let is_target = operation
            .target
            .is_some_and(|target| target.parameter_name() == parameter.name);
        let value = if is_target {
            resource_id
        } else {
            match parameter.scope() {
                Some(ScopeParameter::Organization) => scope.org,
                Some(ScopeParameter::Environment) => Some(scope.single_env()?),
                Some(ScopeParameter::Wallet) => scope.wallet,
                Some(ScopeParameter::Webhook) => scope.webhook,
                Some(ScopeParameter::Environments) | None => None,
            }
        }
        .ok_or_else(|| {
            let command = operation.command.join(" ");
            match parameter.scope() {
                Some(ScopeParameter::Organization) => {
                    Error::usage(format!("--org is required for {command}"))
                        .with_hint("Pass --org UUID or select a profile with --profile NAME.")
                }
                Some(ScopeParameter::Wallet) => {
                    Error::usage(format!("--wallet is required for {command}"))
                }
                Some(ScopeParameter::Webhook) => {
                    Error::usage(format!("--webhook is required for {command}"))
                }
                Some(ScopeParameter::Environment | ScopeParameter::Environments) | None => {
                    Error::usage(format!("Missing {} for {command}", parameter.name))
                }
            }
        })?;
        path = path.replace(&format!("{{{}}}", parameter.name), &value.to_string());
    }
    Ok(path)
}

/// Encode documented filters from the scope and generated flags, then `--query` overrides.
fn query(
    operation: &Operation,
    filters: &[Filter],
    overrides: &[QueryOverride],
    scope: &Scope,
) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for parameter in operation.query_filters() {
        let values: Vec<String> = match parameter.scope() {
            Some(ScopeParameter::Environment | ScopeParameter::Environments) => {
                scope.envs.iter().map(Uuid::to_string).collect()
            }
            Some(ScopeParameter::Wallet) => scope.wallet.iter().map(Uuid::to_string).collect(),
            Some(ScopeParameter::Organization | ScopeParameter::Webhook) => Vec::new(),
            None => filters
                .iter()
                .find(|filter| std::ptr::eq(filter.parameter, parameter))
                .map(|filter| filter.values.clone())
                .unwrap_or_default(),
        };
        if parameter.scope() == Some(ScopeParameter::Environment) && values.len() > 1 {
            return Err(Error::usage(
                "This operation accepts only one environment filter",
            ));
        }
        for value in values {
            serialize_query(parameter, &value, &mut pairs)?;
        }
    }
    for QueryOverride { name, value } in overrides {
        let root = name.split('[').next().unwrap_or(name);
        let parameter = operation
            .query_filters()
            .find(|parameter| parameter.name == root)
            .ok_or_else(|| Error::usage(format!("Unknown query parameter {name}")))?;
        if parameter.scope().is_some() {
            return Err(Error::usage(
                "Use --env or --wallet for scope filters so profile conflicts can be checked",
            ));
        }
        if name.contains('[') {
            pairs.push((name.clone(), value.clone()));
        } else {
            serialize_query(parameter, &contract_value(parameter, value)?, &mut pairs)?;
        }
    }
    if operation.has_parameter(PAGINATION_PARAMETER)
        && !pairs
            .iter()
            .any(|(name, _)| name == PAGINATION_PARAMETER || name == OFFSET_PARAMETER)
    {
        pairs.push((
            PAGINATION_PARAMETER.into(),
            PaginationMode::Cursor.as_str().into(),
        ));
    }
    Ok(pairs)
}

/// A `--query` value for a closed parameter, spelled as the contract spells it.
fn contract_value(parameter: &Parameter, value: &str) -> Result<String> {
    if parameter.values.is_empty() {
        return Ok(value.to_owned());
    }
    parameter
        .values
        .iter()
        .find(|allowed| allowed.eq_ignore_ascii_case(value))
        .cloned()
        .ok_or_else(|| {
            Error::usage(format!(
                "Query parameter {} must be one of: {}",
                parameter.name,
                parameter.values.join(", ")
            ))
        })
}

fn serialize_query(
    parameter: &Parameter,
    value: &str,
    out: &mut Vec<(String, String)>,
) -> Result<()> {
    if parameter.is_object() {
        let (key, value) = value
            .split_once('=')
            .ok_or_else(|| Error::usage("Metadata filters require KEY=VALUE"))?;
        out.push((format!("{}[{key}]", parameter.name), value.into()));
    } else if parameter.is_array() {
        out.push((format!("{}[]", parameter.name), value.into()));
    } else {
        out.push((parameter.name.clone(), value.into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        cli::{Command, Invocation},
        registry::operation,
    };
    use serde_json::json;

    const ORG: &str = "11111111-1111-4111-8111-111111111111";
    const ENV: &str = "22222222-2222-4222-8222-222222222222";
    const WALLET: &str = "33333333-3333-4333-8333-333333333333";

    fn scope() -> Scope {
        Scope {
            org: Some(ORG.parse().unwrap()),
            envs: vec![ENV.parse().unwrap()],
            wallet: Some(WALLET.parse().unwrap()),
            webhook: None,
            account: None,
        }
    }

    fn build(args: &[&str]) -> Result<Request> {
        let mut full = vec!["voltage"];
        full.extend_from_slice(args);
        let invocation = Invocation::try_parse_from(full).unwrap();
        let Command::Api(api) = invocation.command else {
            panic!("expected an API command");
        };
        Request::build(&api, &scope())
    }

    #[test]
    fn raw_sends_require_confirmation() {
        let create_payment = operation(OperationId::CreatePayment);
        let send = Request {
            path: String::new(),
            query: Vec::new(),
            body: Some(json!({"type":"bolt11"})),
            body_id: None,
            resource_id: None,
        };
        assert!(send.is_consequential(create_payment));
        let receive = Request {
            body: Some(json!({"payment_kind":"bolt11"})),
            ..send
        };
        assert!(!receive.is_consequential(create_payment));
        let mixed = Request {
            body: Some(json!({"payment_kind":"bolt11","data":{}})),
            ..receive
        };
        assert!(mixed.is_consequential(create_payment));
    }

    #[test]
    fn friendly_receive_builds_the_documented_payload() {
        let request = build(&[
            "payments",
            "receive",
            "--currency",
            "btc",
            "--kind",
            "bolt11",
            "--amount",
            "150",
            "--unit",
            "sats",
            "--id",
            WALLET,
            "--description",
            "coffee",
        ])
        .unwrap();
        assert_eq!(
            request.body.unwrap(),
            json!({
                "id": WALLET, "wallet_id": WALLET, "payment_kind": "bolt11",
                "amount": {"currency": "btc", "amount": 150000}, "description": "coffee"
            })
        );
        assert_eq!(request.resource_id, Some(WALLET.parse().unwrap()));
        assert_eq!(
            request.path,
            format!("/organizations/{ORG}/environments/{ENV}/payments")
        );
    }

    #[test]
    fn friendly_send_builds_the_documented_payload_and_rejects_receive_flags() {
        let request = build(&[
            "payments",
            "send",
            "--currency",
            "btc",
            "--invoice",
            "lnbc1",
            "--max-fee",
            "10",
            "--fee-unit",
            "sats",
            "--metadata",
            "order=1",
        ])
        .unwrap();
        let body = request.body.unwrap();
        assert_eq!(body["type"], "bolt11");
        assert_eq!(body["data"]["payment_request"], "lnbc1");
        assert_eq!(
            body["data"]["max_fee"],
            json!({"currency":"btc","amount":10000})
        );
        assert_eq!(body["metadata"], json!({"order":"1"}));
        assert!(body.get("amount").is_none());
        let error = request_error(&[
            "payments",
            "send",
            "--currency",
            "btc",
            "--invoice",
            "x",
            "--kind",
            "bolt11",
        ]);
        assert_eq!(error, "Send commands do not accept receive flags");
        let error = request_error(&["payments", "send", "--currency", "btc", "--address", "bc1"]);
        assert_eq!(error, "On-chain sends require --amount and --unit");
        let error = request_error(&[
            "payments",
            "send",
            "--currency",
            "usd",
            "--invoice",
            "x",
            "--max-fee",
            "1",
            "--fee-unit",
            "cents",
        ]);
        assert_eq!(error, "Network fees must use BTC units");
        let error = request_error(&[
            "payments",
            "receive",
            "--currency",
            "usd",
            "--kind",
            "bolt11",
            "--amount",
            "1",
            "--unit",
            "sats",
        ]);
        assert_eq!(error, "Receive currency must match the amount unit");
        let error = request_error(&["payments", "receive", "--currency", "btc"]);
        assert_eq!(
            error,
            "--kind is required (or supply the complete --data payload)"
        );
    }

    fn request_error(args: &[&str]) -> String {
        build(args).expect_err("request should fail").message
    }

    #[test]
    fn queries_encode_scope_filters_and_overrides_by_contract_shape() {
        let request = build(&[
            "payments",
            "list",
            "--statuses",
            "completed",
            "--metadata",
            "order=a&b=3",
            "--query",
            "sort_order=desc",
            "--query",
            "limit=5",
        ])
        .unwrap();
        assert_eq!(
            request.query,
            [
                ("wallet_id".to_string(), WALLET.to_string()),
                ("metadata[order]".into(), "a&b=3".into()),
                ("statuses[]".into(), "completed".into()),
                ("sort_order".into(), "DESC".into()),
                ("limit".into(), "5".into()),
                ("pagination".into(), "cursor".into()),
            ]
        );
        assert_eq!(request.pagination_mode(), PaginationMode::Cursor);
        assert_eq!(
            request_error(&["payments", "list", "--query", "wallet_id=x"]),
            "Use --env or --wallet for scope filters so profile conflicts can be checked"
        );
        assert_eq!(
            request_error(&["payments", "list", "--query", "sort_order=sideways"]),
            "Query parameter sort_order must be one of: ASC, DESC"
        );
        assert_eq!(
            request_error(&["payments", "list", "--query", "bogus=1"]),
            "Unknown query parameter bogus"
        );
        let offset = build(&["payments", "list", "--query", "offset=20"]).unwrap();
        assert_eq!(offset.pagination_mode(), PaginationMode::Offset);
        assert_eq!(offset.offset(), Some(20));
    }

    #[test]
    fn friendly_quotes_build_the_documented_payload_and_name_missing_flags() {
        let request = build(&[
            "quotes",
            "create",
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
            "--id",
            ORG,
        ])
        .unwrap();
        assert_eq!(
            request.body.unwrap(),
            json!({
                "id": ORG, "line_of_credit_id": WALLET, "network": "mutinynet",
                "amount": {"currency": "usd", "amount": 1000}, "to": "btc"
            })
        );
        for (args, missing) in [
            (
                vec![
                    "quotes",
                    "create",
                    "--network",
                    "mutinynet",
                    "--amount",
                    "1",
                    "--unit",
                    "usd",
                    "--to",
                    "btc",
                ],
                "credit-line",
            ),
            (
                vec![
                    "quotes",
                    "create",
                    "--credit-line",
                    WALLET,
                    "--amount",
                    "1",
                    "--unit",
                    "usd",
                    "--to",
                    "btc",
                ],
                "network",
            ),
            (
                vec![
                    "quotes",
                    "create",
                    "--credit-line",
                    WALLET,
                    "--network",
                    "mutinynet",
                    "--unit",
                    "usd",
                    "--to",
                    "btc",
                ],
                "amount",
            ),
            (
                vec![
                    "quotes",
                    "create",
                    "--credit-line",
                    WALLET,
                    "--network",
                    "mutinynet",
                    "--amount",
                    "1",
                    "--to",
                    "btc",
                ],
                "unit",
            ),
            (
                vec![
                    "quotes",
                    "create",
                    "--credit-line",
                    WALLET,
                    "--network",
                    "mutinynet",
                    "--amount",
                    "1",
                    "--unit",
                    "usd",
                ],
                "to",
            ),
        ] {
            assert_eq!(
                request_error(&args),
                format!("--{missing} is required (or supply the complete --data payload)")
            );
        }
        assert_eq!(
            request_error(&["webhooks", "test", WALLET]),
            "This operation requires --data @file or --data -"
        );
    }

    #[test]
    fn raw_payloads_must_agree_with_scope_and_carry_payment_identity() {
        let check = |id: OperationId, alias: Option<PaymentDirection>, body: Value| {
            check_payload(operation(id), alias, RawPayload(&body), &scope())
                .err()
                .map(|error| error.message)
        };
        let create_payment = OperationId::CreatePayment;
        assert_eq!(
            check(
                create_payment,
                None,
                json!({"id": ORG, "wallet_id": WALLET, "currency": "btc"})
            ),
            None
        );
        assert_eq!(
            check(
                create_payment,
                None,
                json!({"id": "nope", "wallet_id": WALLET})
            ),
            Some("Expected UUID, got nope".into())
        );
        assert_eq!(
            check(create_payment, None, json!({"id": ORG})),
            Some("The complete JSON payload must include wallet_id".into())
        );
        assert_eq!(
            check(create_payment, None, json!({"id": ORG, "wallet_id": ORG})),
            Some("--wallet conflicts with payload wallet_id".into())
        );
        assert_eq!(
            check(create_payment, None, json!({"wallet_id": WALLET})),
            Some("The complete payment payload requires an id; generate one once and retain it for reconciliation".into())
        );
        assert_eq!(
            check(
                create_payment,
                None,
                json!({"id": ORG, "wallet_id": WALLET, "amount": {"currency": "usd"}})
            ),
            Some("USD payment flows require an explicit quote_id".into())
        );
        assert_eq!(
            check(
                create_payment,
                Some(PaymentDirection::Send),
                json!({"id": ORG, "wallet_id": WALLET, "payment_kind": "bolt11"})
            ),
            Some("payments send cannot submit a receive payload".into())
        );
        assert_eq!(
            check(
                create_payment,
                Some(PaymentDirection::Receive),
                json!({"id": ORG, "wallet_id": WALLET, "type": "bolt11"})
            ),
            Some("payments receive requires a receive payload".into())
        );
        assert_eq!(
            check(OperationId::CreateWallet, None, json!({"name": "w"})),
            Some("Wallet payload requires environment_id".into())
        );
        assert_eq!(
            check(
                OperationId::CreateWallet,
                None,
                json!({"environment_id": ORG})
            ),
            Some("Environment scope conflicts with payload environment_id".into())
        );
        assert_eq!(
            check(
                OperationId::CreateWallet,
                None,
                json!({"environment_id": ENV})
            ),
            None
        );
        assert_eq!(
            check(
                OperationId::CreateSession,
                None,
                json!({"wallet_id": WALLET})
            ),
            None
        );
    }

    #[test]
    fn wallet_mutations_take_their_id_from_the_positional_target() {
        let request = build(&["wallets", "policies", "get", WALLET]).unwrap();
        assert_eq!(
            request.path,
            format!("/organizations/{ORG}/wallets/{WALLET}/policies")
        );
        assert_eq!(request.resource_id, Some(WALLET.parse().unwrap()));
        assert!(request.body.is_none());
        assert!(!request.is_consequential(operation(OperationId::GetWalletPolicies)));
        let delete = build(&["wallets", "delete", WALLET]).unwrap();
        assert!(delete.is_consequential(operation(OperationId::DeleteWallet)));
    }
}
