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

Amounts are decimals in the unit you name (`msats`, `sats`, `btc`, `cents`, `usd`) and are converted to the API's integer base units exactly; a value with more precision than the unit allows is rejected. `--currency` is the wallet currency for sends and the receive currency for open-amount receives. Fee limits cover network and provider fees only. A send notes on stderr that its fee limit excludes processing fees.

Run a change without `--execute` to see exactly what it would send, then add `--execute` to send it. There is no confirmation prompt. See the [safety model](../concepts/safety-model.md).

## Wait for a payment

An accepted submission returns immediately with `outcome: accepted`. To wait, add `--wait ready` (the invoice or address exists) or `--wait completed` (the payment settled). Waiting polls the payment, starting quickly and backing off with jitter, and honors the API's retry hints. It tolerates a payment that is not visible yet and stops at `--timeout`, reporting the payment ID with exit code 5 so you can keep watching:

```sh
voltage payments get PAYMENT_ID --profile prod --wait completed --timeout 120
```

## Show an invoice

For a BOLT11 receive, `--qr` prints the invoice as a terminal QR code and `--copy` puts it on the clipboard as soon as it is ready. Either flag implies `--wait ready`; with `--wait completed` the invoice is shown first and polling continues to settlement.

## Quotes and lines of credit

Quotes apply to USD lines of credit: a USD payment requires `--quote` with a quote for that line of credit.

```sh
voltage quotes create --profile prod --credit-line CREDIT_LINE_ID \
  --network mutinynet --amount 10 --unit usd --to btc --execute
```

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
