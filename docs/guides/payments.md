# Payments

## Receive and send

```sh
voltage payments receive --profile prod --wallet WALLET_ID \
  --currency btc --kind bolt11 --amount 1000 --unit sats --wait ready --execute
voltage payments send --profile prod --wallet WALLET_ID \
  --currency btc --invoice BOLT11_INVOICE --max-fee 10 --fee-unit sats --execute
voltage payments send --profile prod --wallet WALLET_ID \
  --currency btc --address BITCOIN_ADDRESS --amount 1000 --unit sats --execute
```

Amounts are decimals in the unit you name with `--unit` (`msats`, `sats`, `btc`, `cents`, `usd`) and are converted to the API's integer base units exactly; a value with more precision than the unit allows is rejected. Without `--unit`, an amount is in the currency's base unit: msats for BTC and cents for USD, so `--amount 1000` is 1 sat. `--max-fee` without `--fee-unit` is in msats. `--currency` is the wallet currency for sends and the receive currency for open-amount receives. Without it, the payment reads the wallet and uses its line of credit's currency. That read needs a credential, also when the change is only described, and stderr says which currency was used. A `--unit` in another currency is then rejected, and a USD wallet's amount needs `--unit`: a cent is worth more than a sat, so an amount meant in sats would overpay. A wallet that holds an asset needs the complete body with `--data`. Fee limits cover network and provider fees only. A send notes on stderr that its fee limit excludes processing fees.

Run a change without `--execute` to see exactly what it would send, then add `--execute` to send it. There is no confirmation prompt. See the [safety model](../concepts/safety-model.md).

## Wait for a payment

An accepted submission returns immediately with `outcome: accepted`. To wait, add `--wait ready` (the invoice or address exists) or `--wait completed` (the payment settled). Waiting polls the payment, starting quickly and backing off with jitter, and honors the API's retry hints. It tolerates a payment that is not visible yet and stops at `--timeout`, reporting the payment ID with exit code 5 so you can keep watching:

```sh
voltage payments get PAYMENT_ID --profile prod --wait completed --timeout 120
```

## Show an invoice

For a BOLT11 receive, `--qr` prints the invoice as a terminal QR code and `--copy` puts it on the clipboard as soon as it is ready. Either flag implies `--wait ready`; with `--wait completed` the invoice is shown first and polling continues to settlement.

## Quotes and lines of credit

A wallet on a USD line of credit needs a quote for every payment; without one, the API rejects the payment with `quote_required`. `voltage wallets get WALLET_ID` shows the wallet's `line_of_credit_id`, network, and balance currency.

To receive into a USD wallet, quote the USD amount to BTC, wait until `quotes get` shows a `quote` with no `error`, then receive the same USD amount with that quote:

```sh
voltage quotes create --profile prod --credit-line CREDIT_LINE_ID \
  --network mutinynet --amount 10 --unit usd --to btc --execute
voltage quotes get QUOTE_ID --profile prod
voltage payments receive --profile prod --wallet WALLET_ID \
  --currency usd --kind bolt11 --amount 10 --unit usd --quote QUOTE_ID --wait ready --execute
```

A quote converts `--amount` from the currency of its `--unit` into the `--to` currency, so the two must differ. With `--unit` and `--to`, the CLI rejects a quote whose `--unit` is already in the `--to` currency before sending it. A complete `--data` body is sent as is, and the API rejects it with `mismatch_currency`.

The quote must convert to exactly the payment's amount, or the API rejects the payment with `invalid_currency`. `voltage quotes get QUOTE_ID` shows what a quote converts to. To pay a BTC invoice from a USD wallet, quote the invoice's BTC amount with `--unit sats --to usd`. To receive a USD amount, quote it with `--unit usd --to btc`, as above.

A quote is single-use and expires; a quoted receive's expiration is capped by the quote's remaining life.

On-chain and BIP21 payments and treasury movements are features Voltage enables per organization; the API rejects them with `feature_flag_disabled` until then, and the CLI says so.

## Prices

To see what an amount is worth in the other currency, or the current BTC/USD price, use the price service (no credentials needed):

```sh
voltage price
voltage convert 10 usd --to btc
voltage convert 21000 sats --to usd --at 2026-09-01T00:00:00Z
```

A conversion reports the result in every unit of its currency (`msats`, `sats`, and `btc`, or `cents` and `usd`) together with the quote it used, including the minute the service rounded to.

Related: [requests](requests.md), [safety model](../concepts/safety-model.md), [exit codes](../reference/exit-codes.md).
