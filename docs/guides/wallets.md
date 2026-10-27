# Wallets

```sh
voltage wallets list --profile prod
```

A wallet belongs to an organization and one environment. `--profile` (or `--org` and `--env`) selects them; see [log in and use profiles](login-and-profiles.md). Payment commands take the wallet with `--wallet WALLET_ID`, or `VOLTAGE_WALLET_ID` when no `--profile` is selected, and on a terminal they offer a picker when it is missing.

## List wallets in every organization

```sh
voltage wallets list --all-orgs --account NAME
```

With a browser login, `--all-orgs` lists the wallets in every organization the login can see, in one table or JSON list. Each wallet carries `organization_id` and `organization_name`, and the table shows an organization column. The command's filters, such as `--inactive true`, apply in each organization. It ignores the organization and environment of a profile or of `VOLTAGE_ORGANIZATION_ID`, and it cannot be combined with `--org` or `--env`.

The API lists one organization at a time, and each needs its own token. So the CLI lists the login's organizations, then exchanges a token and reads the wallets for each, four organizations at a time. An API key belongs to one organization, so `--all-orgs` refuses it; use `--org ID` instead.

When some organizations fail, the wallets of the others are still written, and the command then fails with the first failure's exit code. The error names each failed organization, and `--json` lists them in `error.detail.failed_organizations`, each with `organization_id`, `name`, `message`, and the failure's `hint` and `detail` when it has them. When every organization fails, nothing is written. When they all fail the same way, as every token exchange does for a revoked login, the command reports that error itself, such as `credential_problem: login_expired` with its `voltage login` hint.

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

On a terminal, `wallets get` first shows the wallet's name, each available balance, labelled `Available` (with a USD estimate for a BTC balance on mainnet, from the price service), and a sparkline of its completed payment volume over the last 14 days. That takes two extra reads, the BTC price on mainnet and the wallet's 100 newest completed payments (payment checks excluded), summed from each payment's amount in the wallet's currency. Each read has 5 seconds at most, and one that fails or is slow leaves its line out. The wallet's fields follow as usual, and redirected or `--json` output is unchanged.

On a terminal, a read of one wallet without `WALLET_ID` or `--wallet`, such as `voltage wallets get --profile prod`, opens the [wallet picker](login-and-profiles.md#pick-the-scope-interactively) and shows the wallet you pick. A change, such as `wallets update` or `wallets delete`, always names its wallet and never offers a picker. Off a terminal, or with `--json` or `--no-input`, a read without a wallet fails as before and names both ways to pass it.

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
