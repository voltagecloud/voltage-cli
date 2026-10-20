//! A friendly payment without `--currency` takes its wallet's currency.
//!
//! The wallet and its line of credit are read with the account credential, in a description as
//! in a real run, so both show the same request. An explicit `--currency` is never replaced and
//! needs no credential. The flag checks that do not depend on the currency run first, so a
//! local mistake fails before any credential is resolved or request is sent. An amount's
//! `--unit` must then be in the wallet's currency, and a USD wallet's amount needs one.

use crate::{
    Error, Result, api,
    auth::Resolver,
    cli::{ApiInvocation, BodySource, FriendlyFlags, GlobalFlags, PaymentFlags},
    config::Scope,
    input::check_payment,
    payment::Currency,
};

pub async fn fill(
    invocation: &mut ApiInvocation,
    global: &GlobalFlags,
    resolver: Resolver<'_>,
    scope: &Scope,
) -> Result<()> {
    let alias = invocation.alias;
    let Some(BodySource::Friendly(FriendlyFlags::Payment(flags))) = &mut invocation.body else {
        return Ok(());
    };
    if flags.currency.is_some() {
        return Ok(());
    }
    // Without a wallet the payment reports that instead; reading needs one.
    let Some(wallet) = scope.wallet else {
        return Ok(());
    };
    // Local mistakes fail before any credential is resolved or request is sent.
    check_payment(flags, alias)?;
    let org = scope.require_org()?;
    let credential = resolver
        .resolve_organization(scope)
        .await
        .map_err(|error| error.and_hint("Or pass --currency to skip reading the wallet."))?;
    let currency = api::wallet_currency(global, credential, org, wallet).await?;
    check_unit(flags, currency)?;
    global.terminal().notice(format!(
        "Using the wallet's currency, {currency}; pass --currency to choose another."
    ));
    flags.currency = Some(currency);
    Ok(())
}

/// An amount's unit, checked against the currency read from its wallet. A unit in another
/// currency would denominate the payment wrongly. A USD amount without `--unit` would be
/// read as cents, and a cent is worth more than a sat, so an amount meant in sats would
/// overpay; it needs the unit named.
fn check_unit(flags: &PaymentFlags, wallet: Currency) -> Result<()> {
    if flags.amount.is_none() {
        return Ok(());
    }
    match flags.unit {
        Some(unit) if unit.currency() != wallet => Err(Error::usage(format!(
            "--unit {} is a {} unit, but this wallet's currency is {wallet}",
            unit.name(),
            unit.currency()
        ))
        .with_hint("Pass a unit in the wallet's currency, or --currency to choose another.")),
        None if wallet == Currency::Usd => Err(Error::usage(
            "This wallet's currency is usd, so --amount needs --unit",
        )
        .with_hint("Pass --unit cents or --unit usd.")),
        _ => Ok(()),
    }
}
