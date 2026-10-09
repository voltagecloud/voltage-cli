<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/assets/voltage-logo-light.svg">
    <img alt="Voltage" src="docs/assets/voltage-logo-dark.svg" width="320">
  </picture>
</p>

<h3 align="center">Bitcoin payments from your terminal.</h3>

<p align="center">
  <a href="https://github.com/voltagecloud/voltage-cli/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/voltagecloud/voltage-cli/actions/workflows/ci.yml/badge.svg"></a>
  <a href="https://github.com/voltagecloud/voltage-cli/releases"><img alt="Release" src="https://img.shields.io/github/v/release/voltagecloud/voltage-cli"></a>
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue"></a>
</p>

`voltage` is the command-line client for the [Voltage API](https://voltageapi.com/v1/docs). Create wallets, receive and send Lightning payments (and on-chain payments, where enabled), and manage quotes, webhooks, and checkout from a terminal, a script, or an AI agent.

- **Safe by default.** A change is only described until you add `--execute`. Every payment carries an ID, and the CLI never resubmits one on its own.
- **Built for scripts.** Tables on a terminal, stable JSON when piped, and exit codes that say what happened.
- **Your credentials stay yours.** Browser login with your usual MFA, kept in your OS credential store by default. Secrets are redacted unless you ask for them.

## Install

Download the archive for macOS, Linux, or Windows from the [releases page](https://github.com/voltagecloud/voltage-cli/releases), extract it, and put `voltage` on your `PATH`. Releases are signed and attested: [verify a download](docs/guides/install.md#verify-a-download).

Or build from source with [Rust](https://rustup.rs):

```sh
cargo install --git https://github.com/voltagecloud/voltage-cli --locked
voltage --version
```

## Quick start

```sh
voltage login
voltage organizations list
voltage environments list --org ORG_ID
voltage profiles create prod --org ORG_ID --env ENV_ID --account you@example.com
voltage wallets list --profile prod
```

Replace `ORG_ID`, `ENV_ID`, and the other placeholders with real UUIDs. Then take a payment:

```sh
voltage payments receive --profile prod --wallet WALLET_ID \
  --currency btc --kind bolt11 --amount 1000 --unit sats --qr --execute
```

The invoice prints as a QR code as soon as it is ready. Leave out `--execute` to see the exact request first.

## Use it in scripts

```sh
voltage payments list --profile prod --json | jq '.data.items[].status'
voltage payments send --profile prod --wallet WALLET_ID \
  --currency btc --invoice BOLT11_INVOICE --max-fee 10 --fee-unit sats --dry-run
VOLTAGE_EXECUTE=1 ./pay-invoices.sh
```

| Exit code | Meaning |
|---|---|
| 0 | Success, or accepted without `--wait` |
| 1 | The API rejected the request or a payment failed |
| 4 | Unknown outcome: check the payment ID before you retry |
| 6 | A change was described but not sent |

All codes are in the [exit code reference](docs/reference/exit-codes.md).

## Documentation

| | |
|---|---|
| [Install](docs/guides/install.md) | Platforms, download verification, completions, credential stores |
| [Log in and use profiles](docs/guides/login-and-profiles.md) | Browser login, API keys, scope, profiles, `voltage context` |
| [Payments](docs/guides/payments.md) | Receive, send, wait, quotes, prices |
| [Requests](docs/guides/requests.md) | Request bodies, filters, paging |
| [Checkout](docs/guides/checkout.md) | Session and stream tokens, events |
| [Scripting](docs/guides/scripting.md) | JSON, `--execute` in scripts, quiet output |
| [Safety model](docs/concepts/safety-model.md) | Describe-by-default, dry runs, payment IDs, Ctrl-C, redaction |
| [Output reference](docs/reference/output.md) | Tables, the JSON envelope, streams |
| [Command reference](docs/commands.md) | Every command and its API route |

Every command also has `--help`. [The docs index](docs/README.md) lists everything.

## Contributing

Report security issues as described in [SECURITY.md](SECURITY.md). Build, test, and release instructions are in [the development guide](docs/development.md). Code conventions are in [the style guide](docs/quality.md).

## License

MIT. See [LICENSE](LICENSE).
