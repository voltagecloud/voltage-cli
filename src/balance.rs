//! The headline `wallets get` leads with on a terminal: the wallet's name, each balance, a
//! USD estimate for a mainnet BTC balance, and a sparkline of the wallet's completed
//! payments over the last two weeks. Each extra read is best effort; one that fails leaves
//! its line out, and the wallet's fields follow as usual.

use crate::{
    output::{epoch_seconds, readable_amount},
    price::UsdPerBtc,
    style::{Depth, Glyphs, Line, Role},
};
use serde_json::{Value, json};
use std::time::Duration;

/// Days the activity sparkline covers, ending today.
pub const ACTIVITY_DAYS: usize = 14;
/// Payments read for the sparkline, newest first.
pub const ACTIVITY_LIMIT: &str = "100";
/// The most each extra read may take, so a slow price service or list cannot hold up the
/// wallet, whatever `--timeout` allows.
pub const READ_LIMIT: Duration = Duration::from_secs(5);
const SPARKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
const ASCII_SPARKS: [&str; 8] = ["_", ".", ",", "-", "~", "=", "+", "#"];
const SECONDS_PER_DAY: i64 = 86_400;

/// A mainnet BTC balance a USD estimate can price: which balance line, and its signed msats.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Priceable {
    pub index: usize,
    pub negative: bool,
    pub msats: u64,
}

#[derive(Debug, Default)]
pub struct Headline {
    pub name: Option<String>,
    /// Each balance's `available` amount, as the API sends it.
    pub balances: Vec<Value>,
    pub priceable: Option<Priceable>,
    /// That balance in USD cents, when the price service answered.
    pub usd_cents: Option<u64>,
    /// The currency the activity sums: the wallet's first balance's.
    pub currency: String,
    /// Completed volume per day in `currency`'s base unit, oldest first.
    pub activity: Option<Vec<u64>>,
}

impl Headline {
    /// What the wallet itself says: its name, balances, and whether a USD estimate fits.
    pub fn from_wallet(wallet: &Value) -> Self {
        let balances: Vec<Value> = wallet
            .get("balances")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|balance| balance.get("available").cloned())
            .collect();
        // Test-network coins have no price, so only mainnet gets an estimate.
        let mainnet = wallet.get("network").and_then(Value::as_str) == Some("mainnet");
        let priceable = balances
            .iter()
            .enumerate()
            .find_map(|(index, balance)| {
                btc_msats(balance).map(|(negative, msats)| Priceable {
                    index,
                    negative,
                    msats,
                })
            })
            .filter(|_| mainnet);
        let currency = balances
            .first()
            .and_then(|balance| balance.get("currency"))
            .and_then(Value::as_str)
            .unwrap_or("btc")
            .to_owned();
        Self {
            name: wallet
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            balances,
            priceable,
            usd_cents: None,
            currency,
            activity: None,
        }
    }

    pub fn price(&mut self, rate: UsdPerBtc) {
        self.usd_cents = self
            .priceable
            .and_then(|priceable| rate.msats_to_cents(priceable.msats).ok());
    }

    /// The headline's lines, ending with a blank line before the wallet's fields.
    pub fn lines(&self, glyphs: Glyphs, depth: Depth) -> Vec<Line> {
        let mut lines = Vec::new();
        if let Some(name) = &self.name {
            lines.push(Line::default().styled_brand(depth, name));
        }
        for (index, balance) in self.balances.iter().enumerate() {
            let Some(amount) = readable_amount(balance) else {
                continue;
            };
            let mut line = Line::default()
                .paint(Role::Muted, "Available ")
                .styled(Role::Emphasis, &amount);
            // The estimate belongs to the balance it priced, wherever that is in the list.
            if let (Some(cents), Some(priceable)) = (self.usd_cents, self.priceable)
                && priceable.index == index
            {
                let usd = readable_amount(&json!({
                    "amount": cents, "currency": "usd", "negative": priceable.negative
                }));
                let about = if glyphs == Glyphs::UNICODE {
                    "≈"
                } else {
                    "~"
                };
                line = line.text(&format!("  {about} {}", usd.unwrap_or_default()));
            }
            lines.push(line);
        }
        if let Some(days) = &self.activity {
            lines.push(
                Line::default()
                    .paint(Role::Muted, "Activity  ")
                    .text(&sparkline(days, glyphs))
                    .text(&format!("  {}", caption(days, &self.currency))),
            );
        }
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines
    }
}

/// A BTC amount in msats, with its sign.
fn btc_msats(amount: &Value) -> Option<(bool, u64)> {
    let negative = amount
        .get("negative")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    (amount.get("currency").and_then(Value::as_str) == Some("btc"))
        .then(|| base_units(amount))
        .flatten()
        .map(|msats| (negative, msats))
}

/// An `Amount` in its currency's base unit (msats for BTC, cents for USD), whatever unit it
/// names. An amount that is not a whole number of base units cannot be summed exactly.
fn base_units(amount: &Value) -> Option<u64> {
    let value = amount.get("amount")?.as_u64()?;
    let currency = amount.get("currency").and_then(Value::as_str)?;
    let factor = match (currency, amount.get("unit").and_then(Value::as_str)) {
        ("btc", None | Some("msats")) | ("usd", None | Some("cents")) => 1,
        ("btc", Some("sats")) => 1_000,
        ("btc", Some("btc")) => 100_000_000_000,
        ("usd", Some("usd")) => 100,
        (_, None) => 1,
        _ => return None,
    };
    value.checked_mul(factor)
}

/// Completed volume per day in `currency`'s base unit for the `ACTIVITY_DAYS` days up to
/// `now`, oldest first, from each payment's `data.amount`. Other statuses and currencies,
/// unreadable amounts, and older payments are left out.
pub fn activity(page: &Value, now: i64, currency: &str) -> Vec<u64> {
    let mut days = vec![0u64; ACTIVITY_DAYS];
    let today = now.div_euclid(SECONDS_PER_DAY);
    let items = page
        .get("items")
        .or(Some(page))
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for payment in items {
        if payment.get("status").and_then(Value::as_str) != Some("completed") {
            continue;
        }
        let volume = payment
            .pointer("/data/amount")
            .filter(|amount| amount.get("currency").and_then(Value::as_str) == Some(currency))
            .and_then(base_units);
        let created = payment
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(epoch_seconds);
        let (Some(volume), Some(created)) = (volume, created) else {
            continue;
        };
        let age = today - created.div_euclid(SECONDS_PER_DAY);
        if let Ok(age) = usize::try_from(age)
            && age < ACTIVITY_DAYS
        {
            let slot = &mut days[ACTIVITY_DAYS - 1 - age];
            *slot = slot.saturating_add(volume);
        }
    }
    days
}

/// One bar per day, scaled to the busiest day; a day with no volume is the lowest bar.
fn sparkline(days: &[u64], glyphs: Glyphs) -> String {
    let bars = if glyphs == Glyphs::UNICODE {
        SPARKS
    } else {
        ASCII_SPARKS
    };
    let busiest = days.iter().copied().max().unwrap_or(0);
    days.iter()
        .map(|&volume| {
            let level = if busiest == 0 || volume == 0 {
                0
            } else {
                // Any volume shows above the floor; the busiest day is the top bar.
                let scaled = u128::from(volume) * 6 / u128::from(busiest);
                1 + usize::try_from(scaled).unwrap_or(6)
            };
            bars[level.min(bars.len() - 1)]
        })
        .collect()
}

fn caption(days: &[u64], currency: &str) -> String {
    let total: u64 = days.iter().fold(0, |sum, day| sum.saturating_add(*day));
    if total == 0 {
        return format!("no completed payments in {ACTIVITY_DAYS} days");
    }
    let total =
        readable_amount(&json!({"amount": total, "currency": currency})).unwrap_or_default();
    format!("{total} completed in {ACTIVITY_DAYS} days")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_792_540_800; // 2026-10-21 00:00:00 UTC

    fn wallet(network: &str) -> Value {
        json!({"name": "Treasury", "network": network, "balances": [
            {"available": {"amount": 150_000_000, "currency": "btc", "unit": "msats", "negative": true}},
            {"available": {"amount": 1234, "currency": "usd"}}
        ]})
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| anstream::adapter::strip_str(line.as_str()).to_string())
            .collect()
    }

    #[test]
    fn a_mainnet_btc_balance_gets_a_usd_estimate_and_a_test_network_does_not() {
        let mut headline = Headline::from_wallet(&wallet("mainnet"));
        assert_eq!(
            headline.priceable,
            Some(Priceable {
                index: 0,
                negative: true,
                msats: 150_000_000
            })
        );
        headline.price(UsdPerBtc::parse("60000").unwrap());
        assert_eq!(
            plain(&headline.lines(Glyphs::UNICODE, Depth::Basic)),
            [
                "Treasury",
                "Available -150,000 sats  ≈ -$90.00",
                "Available $12.34",
                ""
            ]
        );
        let mut test = Headline::from_wallet(&wallet("mutinynet"));
        test.price(UsdPerBtc::parse("60000").unwrap());
        assert_eq!(test.usd_cents, None);
        assert!(!plain(&test.lines(Glyphs::UNICODE, Depth::Basic))[1].contains('≈'));
        // ASCII says "about" with a tilde.
        assert_eq!(
            plain(&headline.lines(Glyphs::ASCII, Depth::Basic))[1],
            "Available -150,000 sats  ~ -$90.00"
        );
        assert!(
            Headline::from_wallet(&json!({}))
                .lines(Glyphs::ASCII, Depth::Basic)
                .is_empty()
        );
    }

    #[test]
    fn the_estimate_stays_on_the_btc_balance_when_a_usd_balance_comes_first() {
        let mut headline = Headline::from_wallet(&json!({"network": "mainnet", "balances": [
            {"available": {"amount": 1234, "currency": "usd"}},
            {"available": {"amount": 2_000_000, "currency": "btc", "unit": "sats"}}
        ]}));
        assert_eq!(headline.priceable.map(|priceable| priceable.index), Some(1));
        headline.price(UsdPerBtc::parse("60000").unwrap());
        assert_eq!(
            plain(&headline.lines(Glyphs::UNICODE, Depth::Basic)),
            [
                "Available $12.34",
                "Available 2,000,000 sats  ≈ $1,200.00",
                ""
            ]
        );
        assert_eq!(headline.currency, "usd");
    }

    #[test]
    fn activity_sums_each_payment_amount_in_the_wallet_currency_by_day() {
        let page = json!({"items": [
            // Lightning in msats, on-chain in sats, and plain BTC: each counts in msats.
            {"status": "completed", "created_at": "2026-10-20T23:59:59Z",
                "data": {"amount": {"amount": 1000, "currency": "btc", "unit": "msats"}}},
            {"status": "completed", "created_at": "2026-10-20T01:00:00Z",
                "data": {"amount": {"amount": 2, "currency": "btc", "unit": "sats"}, "amount_sats": 2}},
            {"status": "completed", "created_at": "2026-10-08T00:00:00Z",
                "data": {"amount": {"amount": 500, "currency": "btc"}}},
            // Too old, another currency, not completed, the deprecated field alone, no time.
            {"status": "completed", "created_at": "2026-10-07T23:59:59Z",
                "data": {"amount": {"amount": 9, "currency": "btc"}}},
            {"status": "completed", "created_at": "2026-10-20T01:00:00Z",
                "data": {"amount": {"amount": 7, "currency": "usd"}}},
            {"status": "expired", "created_at": "2026-10-20T01:00:00Z",
                "data": {"amount": {"amount": 7, "currency": "btc"}}},
            {"status": "completed", "created_at": "2026-10-20T01:00:00Z",
                "data": {"amount_msats": 7}},
            {"status": "completed", "created_at": "not a time",
                "data": {"amount": {"amount": 7, "currency": "btc"}}},
            {"status": "completed", "created_at": "2026-10-22T00:00:00Z",
                "data": {"amount": {"amount": 9, "currency": "btc"}}}
        ]});
        let days = activity(&page, NOW + 3_600, "btc");
        // Today (2026-10-21) is last, 2026-10-20 the day before, and 2026-10-08 the first.
        assert_eq!(days.len(), ACTIVITY_DAYS);
        assert_eq!(days[ACTIVITY_DAYS - 1], 0);
        assert_eq!(days[ACTIVITY_DAYS - 2], 3000);
        assert_eq!(days[0], 500);
        assert_eq!(days.iter().sum::<u64>(), 3500);
        assert_eq!(activity(&page, NOW + 3_600, "usd")[ACTIVITY_DAYS - 2], 7);
    }

    #[test]
    fn amounts_convert_to_base_units_only_when_exact() {
        let amount = |value: Value| base_units(&value);
        assert_eq!(
            amount(json!({"amount": 3, "currency": "btc", "unit": "btc"})),
            Some(300_000_000_000)
        );
        assert_eq!(
            amount(json!({"amount": 3, "currency": "usd", "unit": "usd"})),
            Some(300)
        );
        assert_eq!(amount(json!({"amount": 3, "currency": "eur"})), Some(3));
        assert_eq!(
            amount(json!({"amount": 3, "currency": "btc", "unit": "bits"})),
            None
        );
        assert_eq!(amount(json!({"amount": 1.5, "currency": "btc"})), None);
    }

    #[test]
    fn the_sparkline_scales_to_the_busiest_day_and_floors_empty_days() {
        assert_eq!(sparkline(&[0, 1, 50, 100], Glyphs::UNICODE), "▁▂▅█");
        assert_eq!(sparkline(&[0, 0], Glyphs::UNICODE), "▁▁");
        assert_eq!(sparkline(&[0, 100], Glyphs::ASCII), "_#");
        assert_eq!(caption(&[0, 0], "btc"), "no completed payments in 14 days");
        assert_eq!(
            caption(&[1_000_000, 500_000], "btc"),
            "1,500 sats completed in 14 days"
        );
        assert_eq!(caption(&[1234], "usd"), "$12.34 completed in 14 days");
        let headline = Headline {
            activity: Some(vec![0, 100]),
            currency: "btc".into(),
            ..Headline::default()
        };
        assert_eq!(
            plain(&headline.lines(Glyphs::UNICODE, Depth::Basic)),
            ["Activity  ▁█  0.1 sats completed in 14 days", ""]
        );
    }
}
