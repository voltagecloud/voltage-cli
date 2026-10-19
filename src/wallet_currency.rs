//! A friendly payment without `--currency` takes its wallet's currency.
//!
//! On a receive, an amount's `--unit` already names the currency, so nothing is read. Otherwise
//! the wallet and its line of credit are read with the account credential, in a description as
//! in a real run, so both show the same request. An explicit `--currency` is never replaced and
//! needs no credential. The flag checks that do not depend on the currency run first, so a
//! local mistake fails before any credential is resolved or request is sent.

use crate::{
    Result,
    api::{self, SubmissionState},
    auth,
    cli::{ApiInvocation, BodySource, FriendlyFlags, GlobalFlags},
    config::{Scope, Settings},
    input::check_payment,
    payment::PaymentDirection,
};

pub async fn fill(
    invocation: &mut ApiInvocation,
    global: &GlobalFlags,
    settings: &Settings,
    scope: &Scope,
    submission: &SubmissionState,
) -> Result<()> {
    let alias = invocation.alias;
    let Some(BodySource::Friendly(FriendlyFlags::Payment(flags))) = &mut invocation.body else {
        return Ok(());
    };
    if flags.currency.is_some() {
        return Ok(());
    }
    if alias == Some(PaymentDirection::Receive)
        && let Some(unit) = flags.unit.filter(|_| flags.amount.is_some())
    {
        flags.currency = Some(unit.currency());
        return Ok(());
    }
    // Without a wallet the payment reports that instead; reading needs one.
    let Some(wallet) = scope.wallet else {
        return Ok(());
    };
    // Local mistakes fail before any credential is resolved or request is sent.
    check_payment(flags, alias)?;
    let org = scope.require_org()?;
    let credential = auth::resolve_organization(settings, scope, global, submission)
        .await
        .map_err(|error| error.and_hint("Or pass --currency to skip reading the wallet."))?;
    let currency = api::wallet_currency(global, credential, org, wallet).await?;
    global.terminal().notice(format!(
        "Using the wallet's currency, {currency}; pass --currency to choose another."
    ));
    flags.currency = Some(currency);
    Ok(())
}
