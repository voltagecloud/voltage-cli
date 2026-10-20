# Wallets

```sh
voltage wallets list --profile prod
```

A wallet belongs to an organization and one environment. `--profile` (or `--org` and `--env`) selects them; see [log in and use profiles](login-and-profiles.md). Payment commands take the wallet with `--wallet WALLET_ID` or `VOLTAGE_WALLET_ID`, and on a terminal they offer a picker when it is missing.

## Create a wallet

A wallet is backed by a line of credit. Select one environment, then pass the name, the line of credit, the network, and the credit limit in the line of credit's integer base units:

```sh
voltage credit-lines list --profile prod
voltage wallets create --profile prod --name treasury \
  --credit-line CREDIT_LINE_ID --network mutinynet --limit 100000 --execute
```

`--metadata KEY=VALUE` (repeatable) attaches your own labels, and `--id` sets the new wallet's UUID yourself. Without `--execute`, the command only describes the wallet it would create.

## Inspect a wallet

```sh
voltage wallets get WALLET_ID --profile prod
voltage wallets ledger list WALLET_ID --profile prod --all
voltage wallets payments summary WALLET_ID --profile prod
```

The ledger lists every credit, hold, capture, and release on the wallet. `--all` follows every page; with `--output ndjson` each page is written as it arrives.

## Change a wallet

```sh
voltage wallets update WALLET_ID --profile prod --name operations --execute
voltage wallets policies get WALLET_ID --profile prod --json | jq .data > policies.json
voltage wallets policies update WALLET_ID --profile prod --data @policies.json --execute
```

Policies have no friendly flags: save the current policies, edit the file, and send the complete object with `--data`. Save only `.data`: `--json` and `--output-file` wrap the response in an envelope, which is not a policy body. Deleting a wallet cannot be undone, so run it without `--execute` first to see the request:

```sh
voltage wallets delete WALLET_ID --profile prod
voltage wallets delete WALLET_ID --profile prod --execute
```

Related: [payments](payments.md), [requests](requests.md), [Voltage API wallets](https://voltageapi.com/v1/docs).
