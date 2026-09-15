# Voltage CLI

`voltage` provides native command-line access to the [Voltage API](https://voltageapi.com/v1/docs). It includes commands for all 47 operations in the checked-in API contract, account/environment discovery, and OAuth device login.

**Release status:** browser login requires the two auth-service PRs and frontend approval page in [the integration guide](docs/integration.md). Local validation is recorded in [verification](docs/verification.md). Complete staging acceptance before publishing a release.

## Install from source

Install [Rust](https://rustup.rs), then:

```sh
cargo install --path . --locked
voltage --help
```

The repository pins its build toolchain in `rust-toolchain.toml`. On Linux, build with a C compiler, `pkg-config`, and the D-Bus development library (`libdbus-1-dev` on Debian/Ubuntu). At runtime, native credential storage requires an unlocked Secret Service implementation. Use explicit file storage on headless machines.

The release workflow builds archives for Apple Silicon/Intel macOS and ARM64/x86-64 Linux, includes completions and SHA-256 checksums, and generates a Homebrew formula. Release publishing is manual and gated on staging validation. Windows credential support is included in source; Windows packaging is outside this release.

### Staging wrapper

The development staging wrapper pins the API and auth endpoints, isolates CLI state under `.work/staging/cli`, and reads the four Voltage variables in the repository's private `.env` without evaluating it as shell code:

```sh
cp .env.example .env
# Replace placeholders with one matching staging environment and test-network wallet.
chmod 600 .env
./scripts/voltage-staging --build
./scripts/voltage-staging auth status
./scripts/voltage-staging wallets list --json
```

`VOLTAGE_API_KEY`, `VOLTAGE_ORGANIZATION_ID`, and `VOLTAGE_ENVIRONMENT_ID` are required. `VOLTAGE_WALLET_ID` is an optional default for commands accepting `--wallet`; an explicit flag takes precedence. The wrapper refuses endpoint and config-directory overrides so staging commands cannot silently target another deployment. Use `target/debug/voltage` directly for other configurations.

Before creating a payment, inspect the selected wallet and confirm that its reported network is a test network. Then a fixed-amount Lightning receive can be requested with:

```sh
./scripts/voltage-staging payments receive \
  --currency btc --kind bolt11 --amount 150 --unit sats \
  --qr --copy --json
```

## Authenticate

```sh
voltage login
# SSH/headless machines:
voltage login --no-browser --credential-store file
voltage auth status
voltage organizations list
voltage environments list --org ORG_ID
voltage logout
```

Login prints a verification URL and code to stderr. Approve the matching code in your browser after your usual login and MFA. This grants your existing account permissions across its organizations. Profiles do **not** restrict those permissions.

Organization discovery uses the saved login token. Before organization API calls, the CLI exchanges that token for the selected organization's token. Exchange uses the account's saved auth endpoint and preserves its login and refresh credentials. Switching organizations performs a new exchange.

Credentials default to macOS Keychain or Linux Secret Service. There is no automatic plaintext fallback. `--credential-store file` explicitly chooses owner-only files in an owner-only directory. Do not share this directory or commit it to source control. On Windows, use the native credential store.

Save separate accounts with `voltage login --account work`. With multiple saved credentials, select `--account` or a profile. Access tokens refresh automatically under a process-shared lock; refresh tokens rotate. An interrupted refresh may require a new login. `logout` revokes the selected session before removing credentials; failures leave credentials available for another attempt. `logout --local` removes only local credentials. Already-issued access JWTs may remain valid until expiry.

For automation, set `VOLTAGE_API_KEY` through your secret manager. To save an API key without placing it in shell history:

```sh
voltage auth import-key --account staging-key --org ORG_ID --env ENV_ID
# Or pipe your secret manager's output:
secret-manager-command | voltage auth import-key --stdin \
  --account staging-key --org ORG_ID --env ENV_ID --credential-store file
```

API-key scope bindings are supplied when importing and checked locally. Keys from `VOLTAGE_API_KEY` have unknown local scope; the server enforces their permissions. Organization/environment discovery requires a user login.

## Scope and profiles

```sh
voltage wallets list --org ORG_ID --env ENV_ID
voltage wallets get WALLET_ID --org ORG_ID
voltage profiles create staging --org ORG_ID --env ENV_ID --account work
voltage wallets list --profile staging
voltage profiles list
voltage profiles get staging
voltage profiles delete staging
```

Resource IDs are positional. Enclosing scope uses `--org`, `--env`, `--wallet`, and `--webhook`. UUID placeholders in this README must be replaced with real IDs.

An explicit profile selects its organization, environment, and credential together, ignoring ambient scope and API-key variables. Explicit command flags override scope without changing the profile. Known API-key scope mismatches fail before submission. There is no global active profile.

Without a profile, scope flags override `VOLTAGE_ORGANIZATION_ID`, `VOLTAGE_ENVIRONMENT_ID`, and the optional `VOLTAGE_WALLET_ID`. An explicit `--account` selects saved credentials; otherwise `VOLTAGE_API_KEY` wins, then a sole saved credential. Configuration lives in `$XDG_CONFIG_HOME/voltage` or `~/.config/voltage`; override with `VOLTAGE_CONFIG_DIR` or `--config-dir`.

Only endpoints that support environment filtering receive that filter. Organization-wide commands identify that limitation in their help. Wallet mutations validate a supplied environment against the wallet first. An environment's name never determines its network.

## Requests and payments

Every body-bearing operation supports its complete API payload, including undocumented extension fields, without floating-point conversion:

```sh
voltage payments create --profile production --data @payment.json --json --yes
cat payment.json | voltage payments create --profile production --data - --yes
```

Raw bodies and friendly body-building flags are mutually exclusive. Scope flags remain available and conflicting body/scope IDs are rejected. Unknown query parameters are rejected. Repeated documented filters and metadata work through their generated flags or `--query NAME=VALUE`:

```sh
voltage payments list --org ORG_ID --env ENV_ID \
  --statuses completed --statuses failed --metadata order_id=123 --all
```

Common operations have friendly inputs; consult command help for complete flags:

```sh
voltage wallets create --profile staging --name treasury \
  --network mutinynet --credit-line CREDIT_LINE_ID --limit 0
voltage payments receive --profile staging --wallet WALLET_ID \
  --currency btc --kind bolt11 --amount 1000 --unit sats --wait ready
voltage payments send --profile staging --wallet WALLET_ID \
  --currency btc --invoice BOLT11_INVOICE --max-fee 10 --fee-unit sats --yes
voltage payments send --profile staging --wallet WALLET_ID \
  --currency btc --address BITCOIN_ADDRESS --amount 1000 --unit sats --yes
voltage quotes create --profile staging --credit-line CREDIT_LINE_ID \
  --network mutinynet --amount 10 --unit usd --to btc
```

Payment creation requires an explicit wallet. Amounts use checked integer conversion: BTC is represented in millisatoshis and USD in cents. Decimal values are accepted only when exactly representable in the requested unit. `--currency` identifies the send wallet currency; `--unit` identifies the payment amount currency. Network/provider fee limits exclude processing fees. JSON responses retain all original amount and fee fields. USD flows require an explicit `--quote`; use the API's documented quote ID for the selected request.

The contract has a canonical request-shape gap for Taproot Asset sends. Raw documented JSON is available, but no Taproot Asset send convenience builder or verified-support claim is provided.

### Submission and recovery

An empty HTTP 202 means **accepted**, not completed. Use `--wait ready` for an invoice/address becoming available or `--wait completed` for settlement. `receiving` does not count as ready until the payer-facing request is present, and it never counts as completed. Waiting tolerates initial payment projection 404s and stops at `--timeout SECONDS` (default 60). The timeout exit includes the original resource ID.

For BOLT11 receives, `--qr` renders the invoice as a compact terminal QR code and `--copy` copies the original invoice text to the system clipboard as soon as it is ready. Either flag implies `--wait ready` when no explicit wait is supplied; with `--wait completed`, the invoice is presented first and polling then continues through settlement. Waits print status changes and a notice every 15 seconds to stderr. QR and clipboard diagnostics also go to stderr, preserving JSON stdout for scripts. Clipboard unavailability produces a warning without misreporting the accepted payment as failed.

Convenience creates generate a UUID before sending; `--id` preserves a supplied UUID. A private recovery journal under `requests/` records the ID, operation, scope, timestamp, and request hash before submission, without storing the body. Mutation requests are never retried automatically. After an ambiguous payment or treasury submission, the CLI makes one read of the original ID, bounded to five seconds. If the payment is visible, it reports acceptance; otherwise it returns exit 4 and an unknown outcome. An initially missing projection does not prove submission failed. Read the original payment ID before deciding whether to resubmit:

```sh
voltage payments get PAYMENT_ID --profile staging --wait completed --timeout 120
```

Reusing a journaled ID with a different payload is rejected. Keep the original request separately if you need to resubmit it. The journal does not promise server-side idempotency. Ctrl-C exits 130; a submitted payment continues on the server.

Sends, treasury movements, deletes, disabling credit lines, and webhook key rotation require confirmation. Resolved scope and request details go to stderr. Noninteractive execution requires `--yes`, including raw JSON requests.

## Output, secrets, and checkout

TTY output is readable; piped output defaults to JSON. `--json` forces JSON; `--output ndjson` streams page/event envelopes. Diagnostics and prompts go to stderr. The stable result envelope is:

```json
{"http_status":202,"resource_id":"payment-uuid","outcome":"accepted","data":null}
```

API fields remain inside `data`. `--all --json` returns a `data.pages` array of intact page envelopes; NDJSON emits each page as it arrives. Cursor pagination is the default where supported; explicit offset pagination remains available. Use `--timeout` to bound pagination and waits.

Credentials and known secret fields are redacted from normal output, including errors. Operations returning a one-time secret require `--output-file PATH` (new, private file; full JSON) or an explicit `--show-secrets` before making the request. A file must not already exist. Treat arbitrary metadata as potentially sensitive: automatic redaction cannot identify user-defined secrets.

Checkout uses separate credentials and never falls back to your account key. Supply `VOLTAGE_CHECKOUT_TOKEN` for session reads or `VOLTAGE_STREAM_TOKEN` for event watching, or use `--token-file PATH` (private file) / `--token-file -`. Stream-token creation accepts the API's complete `checkout_tokens` body. Supply `--origin https://your-checkout.example` when required by the session. Session projection reads honor retry hints; `checkout events watch` reads SSE and emits event objects, and Ctrl-C exits cleanly. The connection is bounded by `--timeout` and does not reconnect automatically.

## Shell completions

```sh
voltage completions bash > voltage.bash
voltage completions zsh > _voltage
voltage completions fish > voltage.fish
```

Help and completions come from the same command definitions. See [the operation registry](docs/commands.md) for the full command list.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success, or accepted submission without waiting |
| 1 | API or business failure |
| 2 | Invalid invocation or configuration |
| 3 | Authentication or authorization failure |
| 4 | Transport failure or uncertain submission |
| 5 | Wait/pagination timeout |
| 130 | Interrupted |

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
python3 scripts/check-coverage.py
```

Tests use mock HTTP services. They cover all 47 routes and authentication mappings, arbitrary request JSON, query encoding, secret output, confirmation, pagination, payment waiting, uncertainty, and credential lifecycle. CI runs native tests and packaged-binary smoke checks on all required platforms. The versioned API snapshot is `api/openapi.json`; `api/commands.json` explicitly maps operation IDs to human-facing commands. `build.rs` refuses incomplete coverage and generates request metadata. Refresh the snapshot deliberately, then review the command mapping and request tests.

Live payment tests must use an explicitly configured test-network wallet. Routine tests never send a live payment. Infrastructure and unrelated account-management APIs are outside this CLI's scope.
