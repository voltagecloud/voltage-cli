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

## Choose a wallet for an invoice

On a terminal, a payment without `--wallet` offers a wallet picker. Elsewhere, such as with `--json`, `--no-input`, or in a script, `payments send --invoice` without `--wallet` lists the organization's wallets (in the `--env` environment when one is given) and names those that could pay. A wallet qualifies when it is active, is on the invoice's network, holds the `--currency` currency (or BTC or USD without `--currency`), and has funds: its available balance plus its credit limit (`limit`) is positive, since the API lets a payment draw the balance down to minus the limit. A BTC balance's funds must also cover the invoice's amount. A USD balance pays through a quote, so its amount is not compared, and the candidate has `needs_quote: true` and a `quote` command that converts the invoice's amount with `--unit sats --to usd`; add `--quote QUOTE_ID` to the send. The invoice prefix gives the network: `lnbc` is mainnet, `lntb` testnet, `lntbs` signet, which includes mutinynet, and `lnbcrt` regtest, which no wallet uses. The error then exits with code 2. Its hint gives `--wallet ID` with the scope flags for each qualifying wallet, or says why each wallet does not qualify, naming up to five and counting the rest. Without `--env`, each wallet's flags and quote command add that wallet's own `--env`, which the payment and the quote need. An invoice whose prefix or amount the CLI cannot read is not matched, and nothing is listed. `--json` reports the same in `error.detail`:

```json
{"invoice": {"network": "signet", "amount_msats": 1000},
 "candidates": [{"wallet_id": "WALLET_ID", "name": "ops", "network": "mutinynet", "flags": "--wallet WALLET_ID --org ORG_ID", "needs_quote": false}],
 "excluded": [{"wallet_id": "OTHER_ID", "name": "treasury", "network": "mainnet", "reason": "wrong_network"}],
 "basis": "These wallets match the invoice's network and currency and have a balance. ..."}
```

`reason` is `inactive`, `wrong_network`, `wrong_currency`, `no_balance`, or `insufficient_balance` (a BTC wallet whose funds are below the invoice's amount). The list is a shortlist, not a guarantee: fees and line-of-credit rules are checked only when the payment is sent. Listing the wallets needs a credential, also when the change is only described. An on-chain send without `--wallet` reports only the missing flag.

Run a change without `--execute` to see exactly what it would send, then add `--execute` to send it. There is no confirmation prompt. See the [safety model](../concepts/safety-model.md).

## Wait for a payment

An accepted submission returns immediately with `outcome: accepted`. In human output, an accepted receive or quote then names the command that shows it, such as `voltage payments get PAYMENT_ID`. To wait, add `--wait ready` (the invoice or address exists) or `--wait completed` (the payment settled). Waiting polls the payment, starting quickly and backing off with jitter, and honors the API's retry hints. It tolerates a payment that is not visible yet and stops at `--timeout`, reporting the payment ID with exit code 5 so you can keep watching:

```sh
voltage payments get PAYMENT_ID --profile prod --wait completed --timeout 120
```

## Show an invoice

For a BOLT11 receive, `--qr` prints the invoice as a terminal QR code and `--copy` puts it on the clipboard as soon as it is ready. Either flag implies `--wait ready`; with `--wait completed` the invoice is shown first and polling continues to settlement. On a terminal with color the QR code is black on white with the QR spec's four-module quiet zone, so it scans on a dark theme too; with color off (`NO_COLOR`, `--color never`, `TERM=dumb`) it is the compact code, which follows your terminal's theme and may read inverted on a dark one. It appears under a bold `Scan to pay` caption with the amount, memo, and expiry when the payment has them. The caption starts with the network, its dot in the website's color (mainnet orange, mutinynet and signet purple, testnet green); the code itself stays black and white. The CLI reads the payment's wallet for its network, since mutinynet and signet invoices share a prefix, and uses the prefix if that read fails. A wait that ends at `ready` shows the code after the result, so the code stays on screen; with `--wait completed` it comes first, and the status line counts down to the expiry. Redirected stderr gets the compact code it always did.

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
