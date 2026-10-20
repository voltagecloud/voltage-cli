# Changelog

User-visible changes to `voltage`, newest first. Each pull request that changes what users see adds a line under **Unreleased**; see [AGENTS.md](AGENTS.md).

## Unreleased

### Added

- Pick a missing organization, environment, or wallet from a list on a terminal.
- `voltage context` shows the effective profile, scope, credential, API and auth URLs, and where each came from.
- Changes are only described (exit code 6) unless `--execute` or `VOLTAGE_EXECUTE=1` asks to send them; the description is built offline and sends nothing.
- `--no-input` and `--quiet`, with interrupt-safe progress and the time waited so far on payment waits and device approval.
- `voltage --version` shows the git build.
- A payment without `--currency` uses its wallet's line of credit currency, and its `--unit` must be in that currency. An amount without `--unit` is in the currency's base unit (msats or cents), except on a USD wallet read this way, which needs `--unit`. `--max-fee` without `--fee-unit` is in msats.
- Every command that changes something has examples in `--help`, and every command links its guide.
- Guides for wallets, webhooks, troubleshooting, and getting started, and references for global flags, environment variables, and configuration.
- A command that takes a resource ID as an argument also accepts `--id ID`, so the flag that `quotes create` and `payments send` take works there too. It takes one of the two, and its usage line shows both, as in `voltage quotes get <QUOTE_ID|--id <ID>>`.
- In human output, an accepted `payments receive` or `quotes create` names the command that shows the invoice, address, or quote, with the organization, environment, and account that the command used.
- A 403 to `VOLTAGE_API_KEY` says that the key came from that variable and where the organization came from, and suggests `--account NAME` or unsetting the variable. `--json` reports both as `error.detail.credential_sources`.
- The wallet picker shows a wallet's currency when its balances report one, and typing a currency filters on it. A wallet with no balance yet shows no currency.

### Changed

- Human tables fit the terminal width, and all API text is scrubbed of terminal control characters.
- Each item of a string list gets its own line in human output.
- Errors name the next command: missing scope names the command that lists IDs, credential and profile errors list the saved names, HTTP 401 and 403 point to `voltage auth status` and `voltage context`, and `quote_required` points to `voltage quotes create`.
- An API rejection's error leads with the API's reason, and human output shows the rest of the API error as field/value lines instead of JSON.
- `-h` shows usage and options; `--help` adds notes, examples, and the guide link.
- A USD payment without a quote names `--quote QUOTE_ID` and how to create one, and an `invalid_currency` rejection of a quoted amount says how to quote the payment's exact amount.
- `quotes create` rejects a `--unit` already in the `--to` currency before sending it, and its help says which currency each flag names.

### Fixed

- Help and README examples for changes include `--execute`, so copying one sends it.
- `voltage auth import-key` removes the saved key again when it cannot save the configuration, as `voltage login` does, and reports a name in use with the same message.

## 0.1.0 - 2026-09-18

First release: a command for every Voltage API operation, browser login and API keys, profiles, payment waits with a request journal, hosted checkout, prices, and signed release artifacts.
