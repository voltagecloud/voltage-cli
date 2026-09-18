//! Payment values shared by argument parsing, request building, and waiting.
//!
//! Wire encodings here are stable: they appear in request bodies, in the `--wait` and
//! `--unit` flags, and in the payment projections the CLI polls.

use crate::{Error, Result, output::Outcome};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// What `--wait` waits for.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum WaitTarget {
    /// The payer-facing invoice or address exists.
    Ready,
    /// The payment settled.
    Completed,
}

impl WaitTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Completed => "completed",
        }
    }
}

/// Whether a payment moves funds out of or into the wallet.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PaymentDirection {
    Send,
    Receive,
}

/// Currencies the convenience flags can express; `--data` accepts any documented currency.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Currency {
    Btc,
    Usd,
}

/// Unit of a decimal `--amount` or `--max-fee`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum AmountUnit {
    Msats,
    Sats,
    Btc,
    Cents,
    Usd,
}

impl AmountUnit {
    pub fn currency(self) -> Currency {
        match self {
            Self::Msats | Self::Sats | Self::Btc => Currency::Btc,
            Self::Cents | Self::Usd => Currency::Usd,
        }
    }

    /// Decimal places between this unit and the API's integer base unit.
    fn scale(self) -> u32 {
        match self {
            Self::Msats | Self::Cents => 0,
            Self::Usd => 2,
            Self::Sats => 3,
            Self::Btc => 11,
        }
    }
}

/// An exact amount in the API's integer base units: millisatoshis for BTC, cents for USD.
#[derive(Clone, Copy, Debug, Serialize, Eq, PartialEq)]
pub struct Amount {
    pub currency: Currency,
    pub amount: i64,
}

impl Amount {
    /// Convert a nonnegative decimal without exponent notation, rejecting precision the
    /// unit cannot represent and totals beyond the API's int64 range.
    pub fn parse(value: &str, unit: AmountUnit) -> Result<Self> {
        let overflow = || Error::usage("Amount overflow");
        let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
        let digits = |part: &str| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit());
        if !digits(whole) || (value.contains('.') && !digits(fraction)) {
            return Err(Error::usage(
                "Amount must be a nonnegative decimal without exponent notation",
            ));
        }
        let scale = unit.scale();
        let significant = fraction.trim_end_matches('0');
        let precision = u32::try_from(significant.len()).map_err(|_| overflow())?;
        if precision > scale {
            return Err(Error::usage(
                "Amount has precision smaller than the currency's base unit",
            ));
        }
        let whole = whole.parse::<u128>().map_err(|_| overflow())?;
        let fraction = if significant.is_empty() {
            0
        } else {
            significant
                .parse::<u128>()
                .map_err(|_| overflow())?
                .checked_mul(10u128.pow(scale - precision))
                .ok_or_else(overflow)?
        };
        let base = whole
            .checked_mul(10u128.pow(scale))
            .and_then(|whole| whole.checked_add(fraction))
            .and_then(|base| i64::try_from(base).ok())
            .ok_or_else(|| Error::usage("Amount exceeds the API's int64 range"))?;
        Ok(Self {
            currency: unit.currency(),
            amount: base,
        })
    }
}

/// Receive kinds; Taproot Asset receives exist in raw payloads but have no convenience flags.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ReceiveKind {
    Bolt11,
    Onchain,
    Bip21,
    #[value(skip)]
    Taprootasset,
}

/// Wallet networks the convenience flags can name.
#[derive(Clone, Copy, Debug, Serialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet3,
    Mutinynet,
}

/// Payment states in the contract's `PaymentStatus` schema.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PaymentStatus {
    Sending,
    Receiving,
    Approved,
    Generating,
    Expired,
    Failed,
    Completed,
}

impl PaymentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sending => "sending",
            Self::Receiving => "receiving",
            Self::Approved => "approved",
            Self::Generating => "generating",
            Self::Expired => "expired",
            Self::Failed => "failed",
            Self::Completed => "completed",
        }
    }
}

/// A projection's `status`: a known state, or text the API added after this contract.
/// Unknown states keep a wait polling and are shown verbatim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum StatusText {
    Known(PaymentStatus),
    Unknown(String),
}

impl StatusText {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Known(status) => status.as_str(),
            Self::Unknown(text) => text,
        }
    }

    fn known(&self) -> Option<PaymentStatus> {
        match self {
            Self::Known(status) => Some(*status),
            Self::Unknown(_) => None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct PaymentData {
    #[serde(default)]
    pub payment_request: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
}

/// Read view over a payment projection; unknown fields and shapes stay in the raw body.
#[derive(Debug, Default, Deserialize)]
pub struct PaymentView {
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<StatusText>,
    #[serde(default)]
    pub direction: Option<PaymentDirection>,
    #[serde(default)]
    pub data: PaymentData,
    #[serde(default)]
    pub bip21_uri: Option<String>,
}

/// What a polled projection says about a wait.
#[derive(Debug, Eq, PartialEq)]
pub enum WaitProgress {
    /// The wait target was reached with this outcome.
    Reached(Outcome),
    /// The payment failed or expired.
    Unsuccessful,
    /// Keep polling.
    Pending,
}

impl PaymentView {
    /// A body that is not a payment object reads as a pending projection.
    pub fn from_body(body: &Value) -> Self {
        Self::deserialize(body).unwrap_or_default()
    }

    /// The BOLT11 invoice, once generated.
    pub fn invoice(&self) -> Option<&str> {
        self.data
            .payment_request
            .as_deref()
            .filter(|invoice| !invoice.is_empty())
    }

    /// A receive is ready once the payer-facing request exists.
    fn is_ready(&self) -> bool {
        [
            self.data.payment_request.as_deref(),
            self.data.address.as_deref(),
            self.bip21_uri.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|value| !value.is_empty())
    }

    /// `receiving` counts as ready only with a payer-facing request, and never as completed.
    pub fn progress(&self, until: WaitTarget) -> WaitProgress {
        match self.status.as_ref().and_then(StatusText::known) {
            Some(PaymentStatus::Failed | PaymentStatus::Expired) => WaitProgress::Unsuccessful,
            Some(PaymentStatus::Completed) => WaitProgress::Reached(Outcome::Completed),
            Some(PaymentStatus::Receiving) if until == WaitTarget::Ready && self.is_ready() => {
                WaitProgress::Reached(Outcome::Ready)
            }
            Some(
                PaymentStatus::Sending
                | PaymentStatus::Receiving
                | PaymentStatus::Approved
                | PaymentStatus::Generating,
            )
            | None => WaitProgress::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_money() {
        assert_eq!(
            Amount::parse("0.00000001", AmountUnit::Btc).unwrap().amount,
            1000
        );
        assert_eq!(
            Amount::parse("12.34", AmountUnit::Usd).unwrap().amount,
            1234
        );
        assert!(Amount::parse("0.0001", AmountUnit::Sats).is_err());
        assert!(Amount::parse("9223372036854775808", AmountUnit::Msats).is_err());
        assert_eq!(
            Amount::parse("9223372036854775807", AmountUnit::Msats)
                .unwrap()
                .amount,
            i64::MAX
        );
        assert!(Amount::parse("-1", AmountUnit::Btc).is_err());
        assert!(Amount::parse("1.", AmountUnit::Btc).is_err());
        assert!(Amount::parse(".5", AmountUnit::Btc).is_err());
        assert!(Amount::parse("1e3", AmountUnit::Sats).is_err());
        assert_eq!(
            serde_json::to_value(Amount::parse("1.50", AmountUnit::Usd).unwrap()).unwrap(),
            json!({"currency":"usd","amount":150})
        );
    }

    #[test]
    fn readiness_requires_a_generated_payment_request() {
        let pending =
            PaymentView::from_body(&json!({"status":"receiving","data":{"payment_request":null}}));
        assert_eq!(pending.progress(WaitTarget::Ready), WaitProgress::Pending);
        let ready = PaymentView::from_body(
            &json!({"status":"receiving","data":{"payment_request":"invoice"}}),
        );
        assert_eq!(
            ready.progress(WaitTarget::Ready),
            WaitProgress::Reached(Outcome::Ready)
        );
        assert_eq!(ready.progress(WaitTarget::Completed), WaitProgress::Pending);
        assert_eq!(
            PaymentView::from_body(&json!({"status":"failed"})).progress(WaitTarget::Completed),
            WaitProgress::Unsuccessful
        );
        assert_eq!(
            PaymentView::from_body(&json!({"status":"sending"})).progress(WaitTarget::Completed),
            WaitProgress::Pending
        );
        let future = PaymentView::from_body(&json!({"status":"settling","id":Uuid::nil()}));
        assert_eq!(
            future.progress(WaitTarget::Completed),
            WaitProgress::Pending
        );
        assert_eq!(
            future.status.as_ref().map(StatusText::as_str),
            Some("settling")
        );
        assert_eq!(future.id, Some(Uuid::nil()));
        assert_eq!(
            PaymentView::from_body(&Value::Null).progress(WaitTarget::Ready),
            WaitProgress::Pending
        );
    }

    #[test]
    fn payment_values_keep_their_wire_spelling() {
        assert_eq!(serde_json::to_value(ReceiveKind::Bolt11).unwrap(), "bolt11");
        assert_eq!(serde_json::to_value(Network::Testnet3).unwrap(), "testnet3");
        assert_eq!(serde_json::to_value(Currency::Usd).unwrap(), "usd");
        assert_eq!(
            serde_json::from_value::<ReceiveKind>(json!("taprootasset")).unwrap(),
            ReceiveKind::Taprootasset
        );
        assert_eq!(
            serde_json::from_value::<PaymentDirection>(json!("send")).unwrap(),
            PaymentDirection::Send
        );
        assert_eq!(AmountUnit::Cents.currency(), Currency::Usd);
        assert_eq!(WaitTarget::Ready.as_str(), "ready");
    }
}
