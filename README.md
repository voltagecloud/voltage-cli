# Voltage CLI

`voltage` is the command-line client for the [Voltage API](https://voltageapi.com/v1/docs). It covers wallets, payments, quotes, lines of credit, bills, webhooks, and checkout, and it signs you in through your browser or with an environment API key.

## Install

Download the archive for your platform from the [releases page](https://github.com/voltagecloud/voltage-cli/releases). Each release ships macOS (Apple Silicon and Intel), Linux (x86-64 and ARM64), and Windows (x86-64) archives with shell completions, a signed `SHA256SUMS` file, and a Homebrew formula (`voltage.rb`). Extract the archive and put `voltage` (or `voltage.exe`) on your `PATH`.

### Verify a download

Every archive carries a SLSA build-provenance attestation, and `SHA256SUMS` is signed with Sigstore (keyless; no keys to trust out of band). Before installing, verify both with the [GitHub CLI](https://cli.github.com) and [cosign](https://docs.sigstore.dev/cosign/system_config/installation/):

```sh
# The archive was built by this repository's release workflow from its version tag.
gh attestation verify voltage-*.tar.gz --repo voltagecloud/voltage-cli

# The checksums were signed by that same workflow run.
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
  --certificate-identity-regexp 'https://github.com/voltagecloud/voltage-cli/.github/workflows/release.yml' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

# The archive matches the signed checksums.
sha256sum --check SHA256SUMS --ignore-missing
```

To build from source, install [Rust](https://rustup.rs) and run:

```sh
cargo install --path . --locked
voltage --version
```

The toolchain is pinned in `rust-toolchain.toml`; no system libraries are needed.

Credentials are stored in the macOS Keychain, the Linux Secret Service (over D-Bus, with no system library required), or the Windows Credential Manager. On a headless Linux machine without an unlocked Secret Service, pass `--credential-store file` when you log in or import a key.

## Quick start

```sh
voltage login
voltage organizations list
voltage environments list --org ORG_ID
voltage profiles create prod --org ORG_ID --env ENV_ID --account you@example.com
voltage wallets list --profile prod
```

Replace `ORG_ID`, `ENV_ID`, and the other placeholders in this document with real UUIDs.

## Authenticate

`voltage login` prints a verification URL and a code, opens the URL in your browser, and waits for your approval after your usual login and MFA. Use `--no-browser` on a remote machine and open the URL yourself. The saved login refreshes its tokens automatically; the refresh token rotates.

```sh
voltage login                                   # saved under your email address
voltage login --account work                    # saved under a name of your choice
voltage login --no-browser --credential-store file
voltage auth status                             # identity and expiry, never the secret
voltage logout                                  # revokes the session, then removes it
voltage logout --local                          # removes local credentials only
```

A login grants the permissions your account already has. Profiles do not restrict them.

For automation, use an environment API key. Import one so it never lands in shell history, or pass it through `VOLTAGE_API_KEY`:

```sh
voltage auth import-key --account ci --org ORG_ID --env ENV_ID
secret-manager-command | voltage auth import-key --stdin --account ci --org ORG_ID --env ENV_ID
VOLTAGE_API_KEY=... voltage wallets list --org ORG_ID
```

An imported key remembers the organization and environment it belongs to, and a command that selects a different one fails before any request is sent. `VOLTAGE_API_KEY` is used only when neither `--profile` nor `--account` is given. `organizations list` and `environments list` require a browser login.

With several saved credentials, select one with `--account` or through a profile.

## Scope and profiles

Resource IDs are positional. The enclosing scope comes from `--org`, `--env`, `--wallet`, and `--webhook`:

```sh
voltage wallets list --org ORG_ID --env ENV_ID
voltage wallets get WALLET_ID --org ORG_ID
voltage payments list --org ORG_ID --env ENV_ID --env OTHER_ENV_ID
```

Without a profile, flags override `VOLTAGE_ORGANIZATION_ID`, `VOLTAGE_ENVIRONMENT_ID`, and `VOLTAGE_WALLET_ID`. A profile bundles an organization, an environment, and a credential, and ignores those variables and `VOLTAGE_API_KEY`; flags still override its scope for one command.

```sh
voltage profiles create staging --org ORG_ID --env ENV_ID --account work
voltage profiles list
voltage profiles get staging
voltage profiles delete staging
```

Configuration lives in `$XDG_CONFIG_HOME/voltage` or `~/.config/voltage`; override the directory with `VOLTAGE_CONFIG_DIR` or `--config-dir`. The directory and everything in it are owner-only.

Only endpoints that filter by environment receive `--env`. Organization-wide commands say so in their help, and wallet mutations verify a supplied environment against the wallet before changing anything.

## Commands and help

Commands mirror the API contract; see [the command reference](docs/commands.md) for the full list with routes. Every command and group has descriptive `--help`; body-building commands also show required flag combinations and examples:

```sh
voltage --help
voltage payments --help
voltage payments receive --help
```

Shell completions come from the same definitions:

```sh
voltage completions bash > voltage.bash
voltage completions zsh > _voltage
voltage completions fish > voltage.fish
voltage completions powershell > _voltage.ps1
```

## Requests

Every operation that takes a body accepts its complete API payload from a file or stdin. The JSON is sent exactly as given, including fields the CLI does not know about, and is limited to 16 MiB:

```sh
voltage payments create --profile prod --data @payment.json --yes
cat payment.json | voltage payments create --profile prod --data - --yes
```

Common operations also offer friendly flags (`wallets create`, `wallets update`, `payments receive`, `payments send`, `quotes create`, `webhooks create`, `webhooks update`); `--help` lists them. Friendly flags and `--data` are mutually exclusive, and a payload whose IDs conflict with the selected scope is rejected.

Documented query filters are flags named after the parameter. Parameters with a fixed set of values accept only those values, case-insensitively, and are sent in the contract's spelling. `--query NAME=VALUE` sets any documented parameter and can be repeated:

```sh
voltage payments list --profile prod --statuses completed --statuses failed \
  --metadata order_id=123 --query sort_order=desc --all
```

`--all` follows pagination by cursor, which the API recommends and the CLI requests by default; endpoints that only page by offset are followed by offset and count. The API has deprecated explicit offset paging (`--offset`, `--pagination offset`) where cursors exist, and the CLI warns when you use it. `--timeout SECONDS` (default 60) bounds the whole request, including paging and waits.

## Payments

```sh
voltage payments receive --profile prod --wallet WALLET_ID \
  --currency btc --kind bolt11 --amount 1000 --unit sats --wait ready
voltage payments send --profile prod --wallet WALLET_ID \
  --currency btc --invoice BOLT11_INVOICE --max-fee 10 --fee-unit sats --yes
voltage payments send --profile prod --wallet WALLET_ID \
  --currency btc --address BITCOIN_ADDRESS --amount 1000 --unit sats --yes
voltage quotes create --profile prod --credit-line CREDIT_LINE_ID \
  --network mutinynet --amount 10 --unit usd --to btc
```

Amounts are decimals in the unit you name (`msats`, `sats`, `btc`, `cents`, `usd`) and are converted to the API's integer base units exactly; a value with more precision than the unit allows is rejected. `--currency` is the wallet currency for sends and the receive currency for open-amount receives. Fee limits cover network and provider fees only.

Quotes apply to USD lines of credit: a USD payment requires `--quote` with a quote for that line of credit. On-chain and BIP21 payments and treasury movements are features Voltage enables per organization; the API rejects them with `feature_flag_disabled` until then, and the CLI says so.

To see what an amount is worth in the other currency, or the current BTC/USD price, use the price service (no credentials needed):

```sh
voltage price
voltage convert 10 usd --to btc
voltage convert 21000 sats --to usd --at 2026-09-01T00:00:00Z
```

A conversion reports the result in every unit of its currency (`msats`, `sats`, and `btc`, or `cents` and `usd`) together with the quote it used, including the minute the service rounded to.

An accepted submission returns immediately with `outcome: accepted`. To wait, add `--wait ready` (the invoice or address exists) or `--wait completed` (the payment settled). Waiting polls the payment, starting quickly and backing off with jitter, and honors the API's retry hints. It tolerates a payment that is not visible yet and stops at `--timeout`, reporting the payment ID with exit code 5 so you can keep watching:

```sh
voltage payments get PAYMENT_ID --profile prod --wait completed --timeout 120
```

For a BOLT11 receive, `--qr` prints the invoice as a terminal QR code and `--copy` puts it on the clipboard as soon as it is ready. Either flag implies `--wait ready`; with `--wait completed` the invoice is shown first and polling continues to settlement.

Sends, treasury movements, deletes, webhook key rotation, and disabling a credit line ask for confirmation after printing the resolved scope and request to stderr. Non-interactive use needs `--yes`.

Payments and treasury movements carry an ID, generated for you unless you pass `--id`. Before sending, the CLI records the ID, operation, scope, and request hash (never the body) in a private journal under `requests/`, and it refuses to reuse an ID with a different request. If the connection drops after a submission, the CLI makes one short read of the ID: if the payment is visible, it reports acceptance; otherwise it exits with code 4 and the ID, without resubmitting. Check the ID before deciding to retry. Ctrl-C exits with code 130 and never cancels a submitted payment.

## Output

On a terminal, results are readable tables; when piped, they are JSON. `--json` forces JSON and `--output table|json|ndjson` selects explicitly. The JSON envelope is stable:

```json
{"http_status":202,"data":null,"resource_id":"PAYMENT_ID","outcome":"accepted"}
```

API fields stay inside `data`. With `--all`, JSON output collects the pages into `data.pages`, and NDJSON writes one envelope per page as it arrives. Diagnostics, prompts, and progress go to stderr. Human errors start with `error:` and may include a safe `hint:`; `--json` makes command-line parse errors machine-readable as well as runtime errors. A downstream consumer that closes stdout early is treated as successful pipeline completion.

Known secret fields and the credentials the CLI presented are redacted from normal output and from error details. Operations that return a one-time secret (webhook creation and key rotation, checkout sessions, stream tokens) refuse to run unless you pass `--output-file PATH`, which writes the complete response to a new owner-only file, or `--show-secrets`. Treat your own metadata as potentially sensitive; redaction cannot recognise it.

## Checkout

Checkout commands use their own credentials and never fall back to your account. Provide a session token through `VOLTAGE_CHECKOUT_TOKEN` and a stream token through `VOLTAGE_STREAM_TOKEN`, or read either from a private file or stdin with `--token-file PATH` / `--token-file -`. Pass `--origin https://shop.example.com` when the session requires a browser origin.

```sh
voltage checkout sessions get SESSION_ID --token-file ./session.token
voltage checkout events watch --token-file - < ./stream.token
```

`checkout sessions get` retries while the session projection is being created, following the API's retry hints. `checkout events watch` streams server-sent events as `outcome: event` envelopes until the connection closes or `--timeout` passes; it does not reconnect. Ctrl-C exits cleanly.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success, or an accepted submission without waiting |
| 1 | The API rejected the request or a payment failed |
| 2 | Invalid invocation or configuration |
| 3 | Authentication or authorization failure |
| 4 | Transport failure or a submission with an unknown outcome |
| 5 | A wait or pagination deadline passed |
| 130 | Interrupted |

## Contributing

Report security issues as described in [SECURITY.md](SECURITY.md). Build, test, and release instructions are in [the development guide](docs/development.md); code conventions are in [the style guide](docs/quality.md).

Agent work plans live in `docs/plans/<work-item>/` as Markdown files. This directory is gitignored on purpose: plans are local, disposable handoff notes for chunks of work, not reviewed project documentation or a source of truth. Keep durable decisions and user-facing behavior in tracked docs, code, and tests; create a subfolder per work item and revise or remove it as work progresses.

## License

MIT; see [LICENSE](LICENSE).
