# Getting started

This walks one path from install to a settled test payment on mutinynet, then the same steps in a script. It takes about ten minutes. You need a Voltage account with an organization and an environment.

## 1. Install and sign in

Install the CLI from the [install guide](guides/install.md), then sign in through your browser:

```sh
voltage login --account me
```

## 2. Save your scope as a profile

Find your organization and environment, then save both with your login as a profile:

```sh
voltage organizations list
voltage environments list --org ORG_ID
voltage profiles create test --org ORG_ID --env ENV_ID --account me
voltage context --profile test
```

`voltage context` shows what every command with `--profile test` will use, and where each value came from.

## 3. Find or create a mutinynet wallet

```sh
voltage wallets list --profile test
```

Use a wallet on a BTC line of credit. A wallet on a USD line of credit needs a quote for every payment, so step 4 would fail with [`quote_required`](guides/troubleshooting.md#quote_required).

If there is no such wallet, create one on a BTC line of credit from `credit-lines list`. Check the request with a dry run first, then send it with `--execute`:

```sh
voltage credit-lines list --profile test
voltage wallets create --profile test --name tutorial \
  --credit-line CREDIT_LINE_ID --network mutinynet --limit 100000 --dry-run
voltage wallets create --profile test --name tutorial \
  --credit-line CREDIT_LINE_ID --network mutinynet --limit 100000 --execute
```

## 4. Receive 1,000 sats

Create a Lightning invoice, show it as a QR code, and wait until it is paid. Pay it from any mutinynet wallet or faucet:

```sh
voltage payments receive --profile test --wallet WALLET_ID \
  --currency btc --kind bolt11 --amount 1000 --unit sats --qr --wait completed --execute
```

The command prints the payment when it settles. If the wait times out (exit code 5), keep watching with `voltage payments get PAYMENT_ID --profile test --wait completed`.

## 5. Script it

With `--json`, each command writes one JSON envelope to stdout and nothing else, and the exit code says what happened:

```sh
voltage payments receive --profile test --wallet WALLET_ID --json \
  --currency btc --kind bolt11 --amount 1000 --unit sats --wait ready --execute \
  | jq -r '.data.data.payment_request'
```

Next: [payments](guides/payments.md), [scripting](guides/scripting.md), [troubleshooting](guides/troubleshooting.md).
