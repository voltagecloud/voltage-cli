//! A BOLT11 send without `--wallet`, where no picker is offered, names the wallets that
//! could pay instead of only reporting the missing flag.
//!
//! The invoice's prefix gives its network and amount. One wallet listing in the current scope,
//! the one the picker reads, gives each wallet's network, balances, and credit limit. A wallet
//! is shortlisted when it is active, is on the invoice's network, holds a currency the payment
//! can use, and has funds: its available balance plus its credit limit, which is what the API
//! checks when it holds a payment's amount (`available - amount >= -limit`, in the backend's
//! `Wallet::has_sufficient_balance`). A BTC balance must also cover the invoice's amount; a
//! USD balance pays through a quote, so its amount is not compared. That is a shortlist, not a
//! guarantee: fees and other line-of-credit rules are known only to the API when the payment is
//! sent.

use crate::{
    Error, Result, api,
    auth::Resolver,
    cli::{ApiInvocation, BodySource, FriendlyFlags, GlobalFlags},
    config::{Scope, WALLET_HINT},
    error::ErrorDetail,
    input::{WALLET_REQUIRED, check_payment},
    payment::Currency,
    pick,
};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// How many excluded wallets the hint names; `--json` lists every one.
const EXCLUDED_IN_HINT: usize = 5;

/// What the shortlist does and does not promise, in the hint and in `--json`.
const BASIS: &str = "These wallets match the invoice's network and currency and have funds, counting their credit limit; a BTC wallet's funds also cover the invoice's amount. That is a shortlist, not a guarantee: fees and line-of-credit rules are checked only when the payment is sent.";

pub async fn require(
    invocation: &ApiInvocation,
    global: &GlobalFlags,
    resolver: Resolver<'_>,
    scope: &Scope,
) -> Result<()> {
    let Some(BodySource::Friendly(FriendlyFlags::Payment(flags))) = &invocation.body else {
        return Ok(());
    };
    let Some(invoice) = flags.invoice.as_deref() else {
        return Ok(());
    };
    // A picker already offers the wallets; without an organization there is nothing to list,
    // and the payment reports that instead.
    if scope.wallet.is_some() || scope.org.is_none() || pick::can_ask(global) {
        return Ok(());
    }
    // Local mistakes fail before any credential is resolved or request is sent.
    check_payment(flags, invocation.alias)?;
    let Some(invoice) = Invoice::parse(invoice) else {
        return Err(Error::usage(WALLET_REQUIRED).with_hint(format!(
            "The invoice is not a BOLT11 invoice the CLI can read: it must start with lnbc, lntb, lntbs, or lnbcrt, then an optional amount. No wallet can be matched to it. {WALLET_HINT}"
        )));
    };
    let skip = "Or pass --wallet ID to skip listing wallets.";
    let credential = resolver
        .resolve_organization(scope)
        .await
        .map_err(|error| error.and_hint(skip))?;
    let listed = api::wallets(global, credential, resolver.credential_sources, scope)
        .await
        .map_err(|error| error.and_hint(skip))?;
    let shortlist = Shortlist::new(invoice, flags.currency, &listed, scope);
    Err(Error::usage(WALLET_REQUIRED)
        .with_hint(shortlist.hint())
        .with_detail(ErrorDetail::WalletShortlist(shortlist)))
}

/// The network a BOLT11 invoice's prefix names (BOLT 11 `currency` prefixes).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InvoiceNetwork {
    Mainnet,
    Testnet,
    Signet,
    Regtest,
}

impl InvoiceNetwork {
    /// Longest prefix first, so `tbs` is not read as `tb` and `bcrt` not as `bc`.
    const PREFIXES: [(&'static str, Self); 4] = [
        ("bcrt", Self::Regtest),
        ("bc", Self::Mainnet),
        ("tbs", Self::Signet),
        ("tb", Self::Testnet),
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
            Self::Signet => "signet",
            Self::Regtest => "regtest",
        }
    }

    /// The wallet `network` values (the API's `Network`) that pay on this network. Mutinynet is
    /// a signet. The API has no regtest wallets.
    fn pays(self, wallet: &str) -> bool {
        match self {
            Self::Mainnet => wallet == "mainnet",
            Self::Testnet => wallet == "testnet",
            Self::Signet => matches!(wallet, "signet" | "mutinynet"),
            Self::Regtest => false,
        }
    }
}

/// What an invoice's human-readable part declares.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Invoice {
    pub network: InvoiceNetwork,
    /// `None` for an invoice without an amount.
    pub amount_msats: Option<u64>,
}

impl Invoice {
    /// `ln`, a network prefix, and an optional amount with a multiplier, before the last `1`,
    /// which bech32 data never contains. `None` when any part is malformed.
    fn parse(invoice: &str) -> Option<Self> {
        let lower = invoice.to_ascii_lowercase();
        let (hrp, _) = lower.rsplit_once('1')?;
        let rest = hrp.strip_prefix("ln")?;
        let (network, amount) = InvoiceNetwork::PREFIXES
            .iter()
            .find_map(|(prefix, network)| Some((*network, rest.strip_prefix(prefix)?)))?;
        let amount_msats = match amount {
            "" => None,
            amount => Some(msats(amount)?),
        };
        Some(Self {
            network,
            amount_msats,
        })
    }
}

/// A BOLT11 amount in msats: digits in bitcoin, optionally scaled by `m`, `u`, `n`, or `p`.
/// A pico-bitcoin amount must be a whole number of msats.
fn msats(amount: &str) -> Option<u64> {
    let (digits, multiplier) = match amount.char_indices().last()? {
        (at, unit @ ('m' | 'u' | 'n' | 'p')) => (&amount[..at], Some(unit)),
        _ => (amount, None),
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value: u64 = digits.parse().ok()?;
    match multiplier {
        None => value.checked_mul(100_000_000_000),
        Some('m') => value.checked_mul(100_000_000),
        Some('u') => value.checked_mul(100_000),
        Some('n') => value.checked_mul(100),
        _ => value.is_multiple_of(10).then_some(value / 10),
    }
}

/// Why a listed wallet is not shortlisted, checked in this order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Exclusion {
    Inactive,
    WrongNetwork,
    WrongCurrency,
    NoBalance,
    /// A BTC balance whose funds, counting the credit limit, are below the invoice's amount.
    InsufficientBalance,
}

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub wallet_id: Uuid,
    pub name: String,
    pub network: Option<String>,
    /// The flags to add to the command, with the effective scope.
    pub flags: String,
    /// The wallet pays in USD, so the payment also needs `--quote QUOTE_ID`.
    pub needs_quote: bool,
    /// For a wallet that needs a quote, the command that creates it for the invoice's amount.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Excluded {
    pub wallet_id: Uuid,
    pub name: String,
    pub network: Option<String>,
    pub reason: Exclusion,
}

/// The wallets in scope that could pay an invoice, and why the others cannot.
#[derive(Clone, Debug, Serialize)]
pub struct Shortlist {
    pub invoice: Invoice,
    pub candidates: Vec<Candidate>,
    pub excluded: Vec<Excluded>,
    pub basis: &'static str,
    #[serde(skip)]
    currency: Option<Currency>,
}

impl Shortlist {
    fn new(invoice: Invoice, currency: Option<Currency>, listed: &Value, scope: &Scope) -> Self {
        let items = listed
            .as_array()
            .or_else(|| listed.get("items").and_then(Value::as_array))
            .map(Vec::as_slice)
            .unwrap_or_default();
        let mut shortlist = Self {
            invoice,
            candidates: Vec::new(),
            excluded: Vec::new(),
            basis: BASIS,
            currency,
        };
        for wallet in items {
            let Some(wallet_id) = wallet
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| id.parse().ok())
            else {
                continue;
            };
            let name = text(wallet, "name").unwrap_or("-").to_owned();
            let network = text(wallet, "network").map(str::to_owned);
            match shortlist.qualify(wallet) {
                Ok(currency) => {
                    // Without --env the listing spans the organization, and a payment or a
                    // quote needs the wallet's own environment.
                    let scope = api::scope_flags_in(scope, text(wallet, "environment_id"));
                    let needs_quote = currency != "btc";
                    let quote = needs_quote.then(|| shortlist.quote(wallet, &scope));
                    shortlist.candidates.push(Candidate {
                        wallet_id,
                        name,
                        network,
                        flags: format!("--wallet {wallet_id}{scope}"),
                        needs_quote,
                        quote,
                    });
                }
                Err(reason) => shortlist.excluded.push(Excluded {
                    wallet_id,
                    name,
                    network,
                    reason,
                }),
            }
        }
        shortlist
    }

    /// The currency a wallet would pay in, BTC when it can, or why it cannot pay.
    fn qualify<'a>(&self, wallet: &'a Value) -> std::result::Result<&'a str, Exclusion> {
        if wallet.get("active").and_then(Value::as_bool) == Some(false) {
            return Err(Exclusion::Inactive);
        }
        if !text(wallet, "network").is_some_and(|network| self.invoice.network.pays(network)) {
            return Err(Exclusion::WrongNetwork);
        }
        let balances = wallet
            .get("balances")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let usable: Vec<&Value> = balances
            .iter()
            .filter(|balance| text(balance, "currency").is_some_and(|c| self.usable(c)))
            .collect();
        if usable.is_empty() && !balances.is_empty() {
            return Err(Exclusion::WrongCurrency);
        }
        let limit = wallet.get("limit").and_then(Value::as_u64).unwrap_or(0);
        let funded: Vec<(&str, i128)> = usable
            .iter()
            .filter_map(|balance| Some((text(balance, "currency")?, funds(balance, limit)?)))
            .filter(|(_, funds)| *funds > 0)
            .collect();
        if funded.is_empty() {
            return Err(Exclusion::NoBalance);
        }
        // Only a BTC balance is in the invoice's unit; a USD balance pays through a quote.
        let covers = |(currency, funds): &&(&str, i128)| {
            *currency != "btc"
                || self
                    .invoice
                    .amount_msats
                    .is_none_or(|msats| *funds >= i128::from(msats))
        };
        funded
            .iter()
            .filter(covers)
            .min_by_key(|(currency, _)| *currency != "btc")
            .map(|(currency, _)| *currency)
            .ok_or(Exclusion::InsufficientBalance)
    }

    /// The quote a USD wallet needs to pay the invoice: its BTC amount converted to USD.
    fn quote(&self, wallet: &Value, scope: &str) -> String {
        let credit_line = text(wallet, "line_of_credit_id").unwrap_or("CREDIT_LINE_ID");
        let network = text(wallet, "network").unwrap_or("NETWORK");
        let amount = match self.invoice.amount_msats {
            Some(msats) if msats.is_multiple_of(1000) => format!("{} --unit sats", msats / 1000),
            Some(msats) => format!("{msats} --unit msats"),
            None => "AMOUNT --unit sats".to_owned(),
        };
        format!(
            "voltage quotes create --credit-line {credit_line} --network {network} --amount {amount} --to usd{scope} --execute"
        )
    }

    /// A currency the payment can be sent in: the `--currency` one, or else any the flags name.
    fn usable(&self, currency: &str) -> bool {
        match self.currency {
            Some(wanted) => currency == wanted.to_string(),
            None => matches!(currency, "btc" | "usd"),
        }
    }

    fn currencies(&self) -> String {
        self.currency
            .map_or_else(|| "btc or usd".to_owned(), |currency| currency.to_string())
    }

    fn hint(&self) -> String {
        let network = self.invoice.network.name();
        if !self.candidates.is_empty() {
            let picks: Vec<String> = self
                .candidates
                .iter()
                .map(|candidate| match &candidate.quote {
                    Some(quote) => format!(
                        "`{}` ({}, a USD wallet: it needs a quote, so create one with `{quote}` and add --quote QUOTE_ID)",
                        candidate.flags, candidate.name
                    ),
                    None => format!("`{}` ({})", candidate.flags, candidate.name),
                })
                .collect();
            return format!(
                "{BASIS} Invoice network: {network}. Add one to the command: {}.",
                picks.join(", ")
            );
        }
        if self.excluded.is_empty() {
            return "No wallets are in this scope. Create one with `voltage wallets create`, or choose another scope with --org and --env.".into();
        }
        let mut reasons: Vec<String> = self
            .excluded
            .iter()
            .take(EXCLUDED_IN_HINT)
            .map(|excluded| {
                let why = match excluded.reason {
                    Exclusion::Inactive => "is inactive".to_owned(),
                    Exclusion::WrongNetwork => format!(
                        "is on {}",
                        excluded.network.as_deref().unwrap_or("an unknown network")
                    ),
                    Exclusion::WrongCurrency => format!("holds no {}", self.currencies()),
                    Exclusion::NoBalance => format!("has no {} funds", self.currencies()),
                    Exclusion::InsufficientBalance => {
                        "has too little btc for the invoice's amount".to_owned()
                    }
                };
                format!("{} ({}) {why}", excluded.name, excluded.wallet_id)
            })
            .collect();
        if let Some(more) = self
            .excluded
            .len()
            .checked_sub(EXCLUDED_IN_HINT)
            .filter(|n| *n > 0)
        {
            reasons.push(format!("and {more} more, listed in --json as `excluded`"));
        }
        format!(
            "No wallet in this scope is active on {network} with {} funds: {}. Fund a matching wallet, create one with `voltage wallets create`, or choose another scope with --org and --env.",
            self.currencies(),
            reasons.join("; ")
        )
    }
}

/// A balance's funds in its base unit (msats or cents): its signed available amount plus the
/// wallet's credit limit, which the API lets a payment draw down to.
fn funds(balance: &Value, limit: u64) -> Option<i128> {
    let available = &balance["available"];
    let amount = i128::from(available["amount"].as_u64()?);
    let signed = if available["negative"] == Value::Bool(true) {
        -amount
    } else {
        amount
    };
    Some(signed + i128::from(limit))
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";
    const C: &str = "33333333-3333-4333-8333-333333333333";

    fn invoice(text: &str) -> Option<Invoice> {
        Invoice::parse(text)
    }

    #[test]
    fn invoice_prefixes_name_their_network_and_amount() {
        let at = |network, amount_msats| {
            Some(Invoice {
                network,
                amount_msats,
            })
        };
        assert_eq!(invoice("lnbc1"), at(InvoiceNetwork::Mainnet, None));
        assert_eq!(
            invoice("lnbc2500u1pvjluez"),
            at(InvoiceNetwork::Mainnet, Some(250_000_000))
        );
        assert_eq!(
            invoice("LNTBS10N1QQQ"),
            at(InvoiceNetwork::Signet, Some(1_000))
        );
        assert_eq!(
            invoice("lntb20m1pvjluez"),
            at(InvoiceNetwork::Testnet, Some(2_000_000_000))
        );
        assert_eq!(invoice("lnbcrt1"), at(InvoiceNetwork::Regtest, None));
        assert_eq!(
            invoice("lnbcrt10p1qqq"),
            at(InvoiceNetwork::Regtest, Some(1))
        );
        assert_eq!(
            invoice("lnbc2m1qqq"),
            at(InvoiceNetwork::Mainnet, Some(200_000_000))
        );
        assert_eq!(invoice("lnbc3"), None, "no separator before the data part");
        assert_eq!(
            invoice("lnbc11qqq"),
            at(InvoiceNetwork::Mainnet, Some(100_000_000_000))
        );
    }

    #[test]
    fn malformed_invoices_are_not_matched() {
        for text in [
            "",
            "bc1qaddress",
            "lnxx1qqq",
            "lnbc",
            "lnbcm1qqq",
            "lnbc2x1qqq",
            "lnbc1p1qqq",
            "lnbc99999999999999999991qqq",
        ] {
            assert_eq!(invoice(text), None, "{text}");
        }
    }

    fn wallet(id: &str, network: &str, balances: Value) -> Value {
        json!({"id": id, "name": "w", "active": true, "network": network, "balances": balances})
    }

    fn balance(currency: &str, amount: u64, negative: bool) -> Value {
        json!({"currency": currency, "available": {"amount": amount, "negative": negative}})
    }

    fn reasons(listed: Value, currency: Option<Currency>) -> (Vec<Uuid>, Vec<Exclusion>) {
        let scope = Scope::default();
        let shortlist = Shortlist::new(invoice("lntbs10n1qqq").unwrap(), currency, &listed, &scope);
        (
            shortlist.candidates.iter().map(|c| c.wallet_id).collect(),
            shortlist.excluded.iter().map(|e| e.reason).collect(),
        )
    }

    #[test]
    fn wallets_are_excluded_by_activity_network_currency_then_balance() {
        let funded = json!([balance("btc", 1_000, false)]);
        let mut inactive = wallet(A, "mutinynet", funded.clone());
        inactive["active"] = json!(false);
        assert_eq!(
            reasons(json!([inactive]), None),
            (vec![], vec![Exclusion::Inactive])
        );
        assert_eq!(
            reasons(json!([wallet(A, "mainnet", funded.clone())]), None),
            (vec![], vec![Exclusion::WrongNetwork])
        );
        assert_eq!(
            reasons(json!([{"id": A, "balances": funded}]), None),
            (vec![], vec![Exclusion::WrongNetwork])
        );
        assert_eq!(
            reasons(
                json!([wallet(A, "signet", funded.clone())]),
                Some(Currency::Usd)
            ),
            (vec![], vec![Exclusion::WrongCurrency])
        );
        let asset = json!([balance("asset:00", 5, false)]);
        assert_eq!(
            reasons(json!([wallet(A, "signet", asset)]), None),
            (vec![], vec![Exclusion::WrongCurrency])
        );
        for empty in [
            json!([]),
            json!([balance("btc", 0, false)]),
            json!([balance("btc", 5, true)]),
        ] {
            assert_eq!(
                reasons(json!([wallet(A, "signet", empty.clone())]), None),
                (vec![], vec![Exclusion::NoBalance]),
                "{empty}"
            );
        }
        let usd = json!([balance("usd", 5, false)]);
        assert_eq!(
            reasons(
                json!([
                    wallet(A, "mutinynet", usd.clone()),
                    wallet(B, "signet", funded)
                ]),
                None
            ),
            (vec![uuid(A), uuid(B)], vec![])
        );
        // A listing in a page, and an item without a UUID, are read as the picker reads them.
        assert_eq!(
            reasons(
                json!({"items": [{"id": "x"}, wallet(A, "signet", usd)]}),
                Some(Currency::Usd)
            ),
            (vec![uuid(A)], vec![])
        );
    }

    #[test]
    fn funds_count_the_credit_limit_and_a_btc_wallet_must_cover_the_amount() {
        let with_limit = |available: Value, limit: u64| {
            let mut listed = wallet(A, "signet", json!([available]));
            listed["limit"] = json!(limit);
            json!([listed])
        };
        // The invoice is 1,000 msats. Credit counts as funds, as the API's hold check does.
        assert_eq!(
            reasons(with_limit(balance("btc", 0, false), 1_000), None),
            (vec![uuid(A)], vec![])
        );
        assert_eq!(
            reasons(with_limit(balance("btc", 500, true), 1_500), None),
            (vec![uuid(A)], vec![])
        );
        // Credit already drawn down to its limit leaves nothing.
        assert_eq!(
            reasons(with_limit(balance("btc", 2_000, true), 2_000), None),
            (vec![], vec![Exclusion::NoBalance])
        );
        // Funds below the invoice's amount.
        assert_eq!(
            reasons(with_limit(balance("btc", 400, false), 500), None),
            (vec![], vec![Exclusion::InsufficientBalance])
        );
        // A USD balance pays through a quote, so its amount is not compared with the invoice.
        assert_eq!(
            reasons(
                json!([wallet(A, "signet", json!([balance("usd", 1, false)]))]),
                None
            ),
            (vec![uuid(A)], vec![])
        );
        // With both, a BTC balance that covers the amount pays without a quote.
        let both = json!([balance("usd", 5, false), balance("btc", 1_000, false)]);
        let scope = Scope::default();
        let shortlist = Shortlist::new(
            invoice("lntbs10n1qqq").unwrap(),
            None,
            &json!([wallet(A, "signet", both)]),
            &scope,
        );
        assert!(!shortlist.candidates[0].needs_quote);
        // An invoice without an amount is covered by any funds.
        let open = Shortlist::new(
            invoice("lntbs1qqq").unwrap(),
            None,
            &json!([wallet(A, "signet", json!([balance("btc", 1, false)]))]),
            &scope,
        );
        assert_eq!(open.candidates.len(), 1);
    }

    #[test]
    fn a_usd_candidate_needs_a_quote_for_the_invoices_amount() {
        let scope = Scope {
            org: Some(uuid(B)),
            ..Scope::default()
        };
        let mut usd = wallet(A, "mutinynet", json!([balance("usd", 5, false)]));
        usd["line_of_credit_id"] = json!("LOC");
        let quoted = |text: &str, listed: &Value| {
            let shortlist = Shortlist::new(invoice(text).unwrap(), None, listed, &scope);
            let candidate = &shortlist.candidates[0];
            assert!(candidate.needs_quote);
            (candidate.quote.clone().unwrap(), shortlist.hint())
        };
        let (quote, hint) = quoted("lntbs10n1qqq", &json!([usd.clone()]));
        assert_eq!(
            quote,
            format!(
                "voltage quotes create --credit-line LOC --network mutinynet --amount 1 --unit sats --to usd --org {B} --execute"
            )
        );
        assert!(hint.contains("it needs a quote"), "{hint}");
        assert!(hint.contains(&quote), "{hint}");
        assert!(hint.contains("add --quote QUOTE_ID"), "{hint}");
        let (quote, _) = quoted("lntbs10p1qqq", &json!([usd.clone()]));
        assert!(quote.contains("--amount 1 --unit msats"), "{quote}");
        let (quote, _) = quoted("lntbs1qqq", &json!([usd]));
        assert!(quote.contains("--amount AMOUNT --unit sats"), "{quote}");
        let bare = json!([{"id": A, "name": "w", "balances": [balance("usd", 5, false)], "network": "signet"}]);
        let (quote, _) = quoted("lntbs1qqq", &bare);
        assert!(
            quote.contains("--credit-line CREDIT_LINE_ID --network signet"),
            "{quote}"
        );
        let btc = Shortlist::new(
            invoice("lntbs1qqq").unwrap(),
            None,
            &json!([wallet(A, "signet", json!([balance("btc", 1, false)]))]),
            &scope,
        );
        assert_eq!(
            serde_json::to_value(&btc).unwrap()["candidates"][0],
            json!({"wallet_id": A, "name": "w", "network": "signet", "flags": format!("--wallet {A} --org {B}"), "needs_quote": false})
        );
    }

    fn uuid(text: &str) -> Uuid {
        text.parse().unwrap()
    }

    #[test]
    fn hints_name_the_flags_to_paste_or_why_no_wallet_qualifies() {
        let scope = Scope {
            org: Some(uuid(B)),
            ..Scope::default()
        };
        let signet = || invoice("lntbs10n1qqq").unwrap();
        let mut named = wallet(A, "signet", json!([balance("btc", 1_000, false)]));
        named["name"] = json!("ops");
        let found = Shortlist::new(signet(), None, &json!([named]), &scope);
        let hint = found.hint();
        assert!(hint.starts_with(BASIS), "{hint}");
        assert!(
            hint.contains(&format!("`--wallet {A} --org {B}` (ops)")),
            "{hint}"
        );
        assert_eq!(
            serde_json::to_value(&found).unwrap()["candidates"][0]["flags"],
            format!("--wallet {A} --org {B}")
        );

        let empty = Shortlist::new(signet(), None, &json!([]), &scope);
        assert!(empty.hint().starts_with("No wallets are in this scope."));

        // Without --env the listing spans the organization, so each candidate's flags, and a
        // USD candidate's quote, name the wallet's own environment.
        let mut placed = wallet(A, "signet", json!([balance("usd", 1_000, false)]));
        placed["environment_id"] = json!(C);
        let found = Shortlist::new(signet(), None, &json!([placed]), &scope);
        let candidate = &found.candidates[0];
        assert_eq!(candidate.flags, format!("--wallet {A} --org {B} --env {C}"));
        assert!(
            candidate
                .quote
                .as_deref()
                .unwrap()
                .ends_with(&format!(" --org {B} --env {C} --execute")),
            "{candidate:?}"
        );
        // An --env given is kept, not replaced by the wallet's.
        let chosen = Scope {
            org: Some(uuid(B)),
            envs: vec![uuid(A)],
            ..Scope::default()
        };
        let mut placed = wallet(A, "signet", json!([balance("btc", 1_000, false)]));
        placed["environment_id"] = json!(C);
        let found = Shortlist::new(signet(), None, &json!([placed]), &chosen);
        assert_eq!(
            found.candidates[0].flags,
            format!("--wallet {A} --org {B} --env {A}")
        );

        // A long list of excluded wallets is cut short in the hint, and kept whole in --json.
        let many: Vec<Value> = (0..EXCLUDED_IN_HINT + 2)
            .map(|_| wallet(A, "mainnet", json!([])))
            .collect();
        let capped = Shortlist::new(signet(), None, &json!(many), &scope);
        let hint = capped.hint();
        assert_eq!(
            hint.matches("is on mainnet").count(),
            EXCLUDED_IN_HINT,
            "{hint}"
        );
        assert!(
            hint.contains("and 2 more, listed in --json as `excluded`"),
            "{hint}"
        );
        assert_eq!(capped.excluded.len(), EXCLUDED_IN_HINT + 2);
        let exact: Vec<Value> = (0..EXCLUDED_IN_HINT)
            .map(|_| wallet(A, "mainnet", json!([])))
            .collect();
        let hint = Shortlist::new(signet(), None, &json!(exact), &scope).hint();
        assert!(!hint.contains("more, listed"), "{hint}");

        let mut inactive = wallet(B, "signet", json!([]));
        inactive["active"] = json!(false);
        let listed = json!([
            wallet(A, "mainnet", json!([])),
            {"id": A, "name": "n"},
            wallet(A, "signet", json!([balance("btc", 1_000, false)])),
            wallet(A, "signet", json!([])),
            inactive,
        ]);
        let none = Shortlist::new(signet(), Some(Currency::Usd), &listed, &scope).hint();
        assert!(
            none.starts_with("No wallet in this scope is active on signet with usd funds: "),
            "{none}"
        );
        for part in [
            format!("w ({A}) is on mainnet"),
            format!("n ({A}) is on an unknown network"),
            format!("w ({A}) holds no usd"),
            format!("w ({A}) has no usd funds"),
            format!("w ({B}) is inactive"),
        ] {
            assert!(none.contains(&part), "{part}: {none}");
        }
        let any = Shortlist::new(
            signet(),
            None,
            &json!([wallet(A, "signet", json!([]))]),
            &scope,
        );
        assert!(any.hint().contains("has no btc or usd funds"));
    }

    #[test]
    fn the_shortlist_keeps_its_documented_shape() {
        let listed = json!([wallet(A, "signet", json!([]))]);
        let shortlist = Shortlist::new(
            invoice("lnbc10u1qqq").unwrap(),
            None,
            &listed,
            &Scope::default(),
        );
        assert_eq!(
            serde_json::to_value(&shortlist).unwrap(),
            json!({
                "invoice": {"network": "mainnet", "amount_msats": 1_000_000},
                "candidates": [],
                "excluded": [{"wallet_id": A, "name": "w", "network": "signet", "reason": "wrong_network"}],
                "basis": BASIS,
            })
        );
    }
}
