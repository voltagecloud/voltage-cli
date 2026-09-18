//! BTC/USD prices from the coinprice service and exact conversions between the two.
//!
//! Quotes only apply to USD lines of credit, so operators often need to know what a USD
//! amount is in sats or what a sats amount is worth. The service publishes one pair,
//! `BTCUSD`, at minute resolution; a conversion carries the quote it used, including the
//! minute the service rounded to, so the figure can be reproduced.

use crate::{
    Error, Result,
    api::{BodyFailure, bounded_body},
    auth,
    payment::{Amount, AmountUnit, Currency},
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const PRICE_URL: &str = "https://coinprice.voltage.cloud";
/// Price documents are tiny; anything larger is not one.
const MAX_PRICE_RESPONSE_BYTES: usize = 64 * 1024;
/// Fractional digits of a price the conversion keeps; the service publishes more than a
/// cent ever needs, and this bound keeps every intermediate product inside `u128`.
const PRICE_FRACTION_DIGITS: u32 = 8;
const MSATS_PER_BTC: u128 = 100_000_000_000;
const CENTS_PER_USD: u128 = 100;

/// The one pair the service publishes.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub enum CurrencyPair {
    #[serde(rename = "BTCUSD")]
    BtcUsd,
}

/// A price as the service published it; `time` is the minute the service rounded to.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PriceQuote {
    pub pair: CurrencyPair,
    pub time: String,
    pub price: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PriceResponse {
    Price(PriceQuote),
    Rejected { error: String },
}

/// USD per BTC as an exact decimal, `mantissa / 10^scale`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsdPerBtc {
    mantissa: u128,
    scale: u32,
}

impl UsdPerBtc {
    /// Parse the service's decimal text, keeping at most eight fractional digits.
    pub fn parse(text: &str) -> Result<Self> {
        let invalid = || Error::api(format!("Invalid price {text}"));
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        let digits = |part: &str| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit());
        if !digits(whole) || (text.contains('.') && !digits(fraction)) {
            return Err(invalid());
        }
        let fraction: String = fraction
            .chars()
            .take(PRICE_FRACTION_DIGITS as usize)
            .collect();
        let scale = u32::try_from(fraction.len()).map_err(|_| invalid())?;
        let mantissa = format!("{whole}{fraction}")
            .parse::<u128>()
            .map_err(|_| invalid())?;
        if mantissa == 0 {
            return Err(invalid());
        }
        Ok(Self { mantissa, scale })
    }

    fn denominator(self) -> u128 {
        10u128.pow(self.scale)
    }

    /// `msats` worth of USD in cents, rounded half up.
    pub fn msats_to_cents(self, msats: u64) -> Result<u64> {
        let numerator = u128::from(msats)
            .checked_mul(self.mantissa)
            .and_then(|value| value.checked_mul(CENTS_PER_USD))
            .ok_or_else(overflow)?;
        divide_rounding(numerator, self.denominator() * MSATS_PER_BTC)
    }

    /// `cents` worth of BTC in millisatoshis, rounded half up.
    pub fn cents_to_msats(self, cents: u64) -> Result<u64> {
        let numerator = u128::from(cents)
            .checked_mul(MSATS_PER_BTC)
            .and_then(|value| value.checked_mul(self.denominator()))
            .ok_or_else(overflow)?;
        divide_rounding(numerator, self.mantissa * CENTS_PER_USD)
    }
}

fn overflow() -> Error {
    Error::usage("Amount exceeds the range the conversion can represent")
}

fn divide_rounding(numerator: u128, denominator: u128) -> Result<u64> {
    let rounded = numerator
        .checked_add(denominator / 2)
        .ok_or_else(overflow)?
        / denominator;
    u64::try_from(rounded).map_err(|_| overflow())
}

/// A decimal rendering of `units` scaled down by `decimals` places, without trailing zeros.
fn decimal(units: u64, decimals: usize) -> String {
    let text = format!("{units:0>width$}", width = decimals + 1);
    let (whole, fraction) = text.split_at(text.len() - decimals);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole}.{fraction}")
    }
}

/// A converted amount in every unit its currency is quoted in.
#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum Converted {
    Bitcoin {
        currency: Currency,
        msats: u64,
        sats: String,
        btc: String,
    },
    Dollars {
        currency: Currency,
        cents: u64,
        usd: String,
    },
}

impl Converted {
    fn bitcoin(msats: u64) -> Self {
        Self::Bitcoin {
            currency: Currency::Btc,
            msats,
            sats: decimal(msats, 3),
            btc: decimal(msats, 11),
        }
    }

    fn dollars(cents: u64) -> Self {
        Self::Dollars {
            currency: Currency::Usd,
            cents,
            usd: decimal(cents, 2),
        }
    }
}

/// A conversion and the quote that produced it.
#[derive(Clone, Debug, Serialize)]
pub struct Conversion {
    pub from: Amount,
    pub to: Converted,
    pub price: PriceQuote,
}

/// The price with the sats one dollar buys, which is what a quote is judged against.
#[derive(Clone, Debug, Serialize)]
pub struct PriceReport {
    #[serde(flatten)]
    pub quote: PriceQuote,
    pub sats_per_usd: String,
}

/// A conversion request: an amount in one currency, converted into the other.
#[derive(Clone, Debug)]
pub struct ConversionRequest {
    pub amount: String,
    pub unit: AmountUnit,
    pub to: Currency,
}

pub struct PriceService {
    client: reqwest::Client,
    base: String,
}

impl PriceService {
    pub fn new(base: &str, timeout: Duration) -> Result<Self> {
        Ok(Self {
            client: auth::client(timeout)?,
            base: auth::base_url(base)?,
        })
    }

    /// The BTC/USD price at `at` (RFC 3339, second resolution, UTC) or now.
    pub async fn quote(&self, at: Option<&str>) -> Result<PriceQuote> {
        let time = match at {
            Some(at) => validated_time(at)?,
            None => "now",
        };
        let response = self
            .client
            .get(format!("{}/currency/BTCUSD/{time}", self.base))
            .send()
            .await
            .map_err(|_| Error::transport("Price request failed; check the price URL"))?;
        let status = response.status().as_u16();
        let body = bounded_body(response, MAX_PRICE_RESPONSE_BYTES)
            .await
            .map_err(|failure| match failure {
                BodyFailure::TooLarge => Error::transport("Price response exceeds 64 KiB"),
                BodyFailure::Interrupted => Error::transport("Price response was interrupted"),
            })?;
        let parsed: PriceResponse = serde_json::from_slice(&body).map_err(|_| {
            Error::api(format!(
                "Price service returned HTTP {status} without a price"
            ))
        })?;
        match parsed {
            PriceResponse::Price(quote) if (200..300).contains(&status) => Ok(quote),
            PriceResponse::Price(_) => {
                Err(Error::api(format!("Price service returned HTTP {status}")))
            }
            PriceResponse::Rejected { error } => Err(Error::api(format!(
                "Price service rejected the request: {error}"
            ))),
        }
    }

    pub async fn report(&self, at: Option<&str>) -> Result<PriceReport> {
        let quote = self.quote(at).await?;
        let rate = UsdPerBtc::parse(&quote.price)?;
        let sats_per_usd = decimal(rate.cents_to_msats(CENTS_PER_USD as u64)?, 3);
        Ok(PriceReport {
            quote,
            sats_per_usd,
        })
    }

    pub async fn convert(
        &self,
        request: &ConversionRequest,
        at: Option<&str>,
    ) -> Result<Conversion> {
        let from = Amount::parse(&request.amount, request.unit)?;
        if from.currency == request.to {
            return Err(Error::usage(
                "--to must name the other currency; the amount is already in it",
            ));
        }
        let quote = self.quote(at).await?;
        let rate = UsdPerBtc::parse(&quote.price)?;
        let units = u64::try_from(from.amount).map_err(|_| overflow())?;
        let to = match request.to {
            Currency::Btc => Converted::bitcoin(rate.cents_to_msats(units)?),
            Currency::Usd => Converted::dollars(rate.msats_to_cents(units)?),
        };
        Ok(Conversion {
            from,
            to,
            price: quote,
        })
    }
}

/// The service parses RFC 3339 and silently answers with the current price when it cannot,
/// so the CLI insists on the exact `YYYY-MM-DDTHH:MM:SSZ` form instead.
fn validated_time(at: &str) -> Result<&str> {
    let bytes = at.as_bytes();
    let well_formed = bytes.len() == 20
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            10 => *byte == b'T',
            13 | 16 => *byte == b':',
            19 => *byte == b'Z',
            _ => byte.is_ascii_digit(),
        });
    if !well_formed {
        return Err(Error::usage(
            "--at must be an RFC 3339 UTC timestamp such as 2026-09-18T17:30:00Z",
        ));
    }
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn prices_parse_as_exact_decimals_with_bounded_precision() {
        let rate = UsdPerBtc::parse("80727.836666666667").unwrap();
        assert_eq!(rate.mantissa, 8_072_783_666_666);
        assert_eq!(rate.scale, 8);
        assert_eq!(UsdPerBtc::parse("50000").unwrap().scale, 0);
        for bad in ["", "abc", "1.", ".5", "0", "0.0", "-1"] {
            assert!(UsdPerBtc::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn conversions_round_half_up_in_both_directions() {
        let rate = UsdPerBtc::parse("50000").unwrap();
        assert_eq!(rate.cents_to_msats(100).unwrap(), 2_000_000);
        assert_eq!(rate.msats_to_cents(2_000_000).unwrap(), 100);
        assert_eq!(rate.msats_to_cents(1).unwrap(), 0);
        assert_eq!(rate.msats_to_cents(1_000_000).unwrap(), 50);
        let odd = UsdPerBtc::parse("33333.33").unwrap();
        assert_eq!(odd.cents_to_msats(1).unwrap(), 30_000);
        assert_eq!(odd.msats_to_cents(30_000).unwrap(), 1);
        assert!(rate.cents_to_msats(u64::MAX).is_err());
    }

    #[test]
    fn converted_amounts_show_sats_and_btc_beside_msats() {
        assert_eq!(
            serde_json::to_value(Converted::bitcoin(12_387_500)).unwrap(),
            json!({"currency":"btc","msats":12387500,"sats":"12387.5","btc":"0.000123875"})
        );
        assert_eq!(
            serde_json::to_value(Converted::dollars(8072)).unwrap(),
            json!({"currency":"usd","cents":8072,"usd":"80.72"})
        );
        assert_eq!(decimal(100_000_000_000, 11), "1");
        assert_eq!(decimal(5, 2), "0.05");
        assert_eq!(decimal(0, 3), "0");
    }

    #[test]
    fn timestamps_must_be_exact_rfc3339_utc() {
        assert!(validated_time("2026-09-18T17:30:00Z").is_ok());
        for bad in [
            "2026-09-18",
            "2026-09-18T17:30:00",
            "2026-09-18T17:30:00+02:00",
            "now",
        ] {
            assert!(validated_time(bad).is_err(), "{bad}");
        }
    }

    /// The coinprice contract snapshot; the price commands rely on this route and shape.
    #[test]
    fn the_cli_uses_only_the_route_and_fields_in_the_coinprice_contract() {
        let spec: Value =
            serde_json::from_str(include_str!("../api/coinprice-openapi.json")).unwrap();
        assert!(spec["paths"]["/currency/{pair}/{time}"]["get"].is_object());
        let price = &spec["components"]["schemas"]["CurrencyPrice"];
        for field in ["pair", "time", "price"] {
            assert!(
                price["properties"].get(field).is_some(),
                "CurrencyPrice lacks {field}"
            );
        }
        assert_eq!(price["properties"]["price"]["type"], "string");
        assert_eq!(
            spec["components"]["schemas"]["CurrencyPair"]["enum"],
            json!(["BTCUSD"])
        );
        assert!(
            spec["components"]["schemas"]["ErrorResponse"]["properties"]
                .get("error")
                .is_some()
        );
        let quote: PriceQuote = serde_json::from_value(
            json!({"pair":"BTCUSD","time":"2026-09-18T17:30:00Z","price":"80727.836666666667"}),
        )
        .unwrap();
        assert_eq!(quote.pair, CurrencyPair::BtcUsd);
    }
}
