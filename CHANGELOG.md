# Changelog

User-visible changes to `voltage`, newest first. Each pull request that changes what users see adds a line under **Unreleased**; see [AGENTS.md](AGENTS.md).

## Unreleased

- `ctutils` 0.4.5, whose constant-time comparisons no longer branch on secret values (RUSTSEC-2026-0385). It reaches the CLI through `sha2` and `digest`.
- Filter `payments list` with `--check-only true|false`, and `wallets ledger list` with `--event-type adjustment`, from the refreshed API contract. Omit `--event-type` for payment ledger entries. Filters carry across pagination.
- `webhooks test WEBHOOK_ID` without `--data` sends a test event, with a new `delivery_id` and a payload of type `test` and event `created`. Before, it failed with "This operation requires --data". `--data` still sends your own payload.
- A list that returns every item in one response, such as `wallets list`, accepts `--all` and ignores it, because there are no more pages. Before, it rejected `--all`. `--all` still pages through lists that take `--limit`.
- A command that acts on one wallet, such as `wallets get`, also takes the wallet as `--wallet ID`. If the argument and `--wallet` name different wallets, the command stops. Without either, the error names both ways to pass it. A profile's or `VOLTAGE_WALLET_ID`'s wallet is never used, so a delete always names its wallet.
- A request that gets no response says why: a timeout (for the auth service, its fixed 30-second limit, which `--timeout` does not change), a connection not made within the 10-second connect limit (which `--timeout` does not change), a failed TLS connection, a connection that could not be made, or one that closed early. A hint names the next step. A write that timed out or closed early may have been applied, so its hint says to look up the resource ID before running the command again, never to retry. This covers API, auth, price, and checkout stream requests, which before said only that the request failed.
- `--output-file` on a path that already exists says so and suggests a new path, instead of showing an OS error; any other refusal keeps its cause and gets the same hint. Both say that no change was sent.
- `-o` is short for `--output`, as in `voltage wallets list -o json`.
- Color on a terminal: help, `error:`, `hint:`, and `details:` labels. `--color auto|always|never` (or `--no-color`) decides, else `NO_COLOR`, `CLICOLOR_FORCE`, and `CLICOLOR`. JSON is never colored.
- An animated spinner on interactive stderr, with the time waited after two seconds; `VOLTAGE_SPINNER=0` keeps it still. A payment wait on a terminal reads as a timeline: one line per status change and a closing `✓` or `✗` line.
- `voltage login` on a terminal shows the verification URL as a clickable link, the code in bold, a countdown to the code's expiry, and a closing `✓ Logged in as EMAIL` line.
- A change described without `--execute` starts, on a terminal, with what it would do and the values it would use (`! Not sent: this would send a payment`) and ends with how to send it, instead of an `error:` line. Exit code 6 is unchanged.
- Human output shows amounts with grouped digits, every BTC amount in sats (`150,000 sats`, `1.5 sats`) and cents as dollars (`$1,234.56`), and table timestamps relative to now (`3 minutes ago`); single resources show the UTC time with the relative time beside it. With color on, headers and field names are bold and known statuses are colored. JSON is unchanged.
- Help headers and usage are Voltage orange where the terminal draws 256 or 24-bit color, and bold elsewhere. `FORCE_COLOR` is honored: `0` or `false` turns color off, other values turn it on when redirected; `--color` and `NO_COLOR` still win.
- A successful `voltage login` on a terminal shows the `voltage` wordmark, a lightning bolt beside the name in Voltage orange, above `✓ Logged in as`.
- `voltage context` and `voltage auth status` draw their fields in a titled card on a terminal, with sources dimmed. Redirected output is unchanged.
- `voltage wallets get` on a terminal leads with the wallet's name, its balances (with a USD estimate on mainnet), and a 14-day sparkline of completed payments.
- `--qr` on a terminal draws the invoice black on white with a quiet zone, so it scans on dark themes, under a `Scan to pay` caption with the amount, memo, and expiry, and after the result so it stays on screen; `--wait completed` counts down to the expiry. The caption leads with the network, its dot in the website's color.
- A payment that settles on a terminal closes with `⚡ Payment ID completed` instead of a check mark, printed after the payment's record so it stays on screen.
- `voltage payments list --watch` follows payments until Ctrl-C: a live table that marks new and changed payments on a terminal, or one `snapshot`, `new`, or `updated` event per payment when piped or with `--json`. `--interval` sets the refresh (default 5 seconds).
- `voltage` with no command, on a terminal, greets you with the wordmark, who you are signed in as, your saved scope, and commands to try; off a terminal it is still a usage error. An empty wallet, payment, or webhook list, and a new profile, end with a `›` line naming the next command.

## 0.2.0 - 2026-10-22

Commands that change something now only describe the change (exit code 6) unless `--execute` or `VOLTAGE_EXECUTE=1` asks to send them: add one of them to scripts that send payments or make other changes. Also new: `voltage context`, interactive pickers, `--no-input` and `--quiet`, and errors that name the next command.

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
- Without a picker, `payments send --invoice` without `--wallet` names the wallets in scope that match the invoice's network and currency and have funds, counting their credit limit, with `--wallet ID` and the scope flags ready to paste, or says why each wallet does not qualify. A BTC wallet's funds must cover the invoice's amount, and a USD wallet comes with the quote it needs. `--json` reports them in `error.detail`. The list is a shortlist, not a guarantee.
- Without `--env`, each wallet the shortlist names comes with its own `--env`, in its flags and its quote command, so the pasted command does not fail for a missing environment. The hint names at most five wallets that do not qualify; `--json` lists all of them.
- `wallets list --all-orgs` lists the wallets in every organization a browser login can see, with an organization column, by reading the organizations in parallel. When some organizations fail, the others are listed and the command then fails, naming each failed organization in `error.detail.failed_organizations`.
- The wallet picker shows a wallet's currency when its balances report one, and typing a currency filters on it. A wallet with no balance yet shows no currency.

### Changed

- On a terminal, `voltage login` offers to save a profile pinned to an organization and environment, and the picker takes the only organization or environment without asking. A wallet is still always asked for. Nothing changes off a terminal, with `--json`, or with `--no-input`.
- `voltage login` under a saved name replaces that login when it is the same user at the same auth URL, and revokes the replaced session. A different user is still refused. With `--account NAME`, a name saved for an API key or for another auth URL is refused before the browser step. An expired or revoked login's hint is now only `voltage login --account NAME`. A saved login whose secret cannot be read is refused, also before the browser step, with a hint that names `voltage logout --local --account NAME`, and `logout --local` removes an account whose secret is already gone or cannot be removed, with a warning for the latter. Without `--credential-store`, a replaced login keeps its credential store, and a new login uses the keychain. `voltage login --json` reports `replaced: true` when the login replaced a saved one. When a replacement cannot be saved, the replaced login stays saved, and the error says so if it could not be restored.
- Human tables fit the terminal width, and all API text is scrubbed of terminal control characters.
- Each item of a string list gets its own line in human output.
- A saved login the auth service no longer accepts reports `credential_problem: login_expired` in `--json` `error.detail`. `voltage context` cannot detect it, because it makes no network request.
- Errors name the next command: missing scope names the command that lists IDs, credential and profile errors list the saved names, HTTP 401 and 403 point to `voltage auth status` and `voltage context`, and `quote_required` points to `voltage quotes create`.
- An API rejection's error leads with the API's reason, and human output shows the rest of the API error as field/value lines instead of JSON.
- `-h` shows usage and options; `--help` adds notes, examples, and the guide link.
- A USD payment without a quote names `--quote QUOTE_ID` and how to create one, and an `invalid_currency` rejection of a quoted amount says how to quote the payment's exact amount.
- `quotes create` rejects a `--unit` already in the `--to` currency before sending it, and its help says which currency each flag names.

### Fixed

- TLS certificate checks reject the distrusted emSign intermediates: `rustls-webpki` 0.103.16 and `webpki-root-certs` 1.0.10 (RUSTSEC-2026-0382, RUSTSEC-2026-0383).
- Help and README examples for changes include `--execute`, so copying one sends it.
- `voltage auth import-key` removes the saved key again when it cannot save the configuration, as `voltage login` does, and reports a name in use with the same message.

## 0.1.0 - 2026-09-18

First release: a command for every Voltage API operation, browser login and API keys, profiles, payment waits with a request journal, hosted checkout, prices, and signed release artifacts.
