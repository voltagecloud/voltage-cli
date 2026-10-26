# Troubleshooting

Each entry is headed by the message or code you see. Errors print `error:` and usually a `hint:` with the next command; with `--json`, they are `error.message` and `error.hint`, and `voltage context` shows the profile, scope, and credential a command would use.

## Exit codes

### Exit code 1

The API rejected the request, or a payment reached a failed state. The message carries the HTTP status, and `--json` adds the API's response under `error.detail.data`. Fix the request, or read the payment with `voltage payments get PAYMENT_ID` before trying again.

### Exit code 2

The command line or configuration is invalid: an unknown flag, a missing `--org`, `--env`, or `--wallet`, an unknown profile, or friendly flags combined with `--data`. The hint names what is missing and how to supply it.

### Exit code 3

Authentication or authorization failed. See [HTTP 401](#http-401) and [HTTP 403](#http-403) below, and the credential entries after them.

### Exit code 4

The request got no response. The message says why: no response within `--timeout` (for the auth service, within its fixed 30 seconds, which `--timeout` does not change), no connection within 10 seconds (an unreachable host or a firewall; `--timeout` does not change this limit), a failed TLS connection (often a certificate this system does not trust, or a proxy that inspects TLS), a connection that could not be made (DNS, a refused port, or a proxy), or a connection that closed early. The hint names the next step. A read can simply be retried, but a change that timed out or whose connection closed early may have been applied, so the hint says to look up its resource ID first: a rerun can repeat the change. For a payment or treasury movement, the request may have been sent: the error reports the ID, and the CLI never resubmits it. Read it with `voltage payments get PAYMENT_ID` before you decide to retry. See [payment IDs](../concepts/safety-model.md#payment-ids-and-the-request-journal).

### Exit code 5

A wait or paging deadline passed. A pending payment is not failed: keep watching it with `voltage payments get PAYMENT_ID --profile prod --wait completed`, or pass a longer `--timeout`.

### Exit code 6

The change was described but not sent, because neither `--execute` nor `VOLTAGE_EXECUTE=1` asked for it. Read the description, then run the same command with `--execute`.

### Exit code 130

You pressed Ctrl-C. Before a change is submitted, nothing was sent. If a payment may have been sent, the message says so and gives its ID; read it before you retry.

## Credentials

### `no_credential`

`error: No credential is available`. Sign in with `voltage login`, import a key with `voltage auth import-key`, or set `VOLTAGE_API_KEY`.

### `multiple_credentials`

`error: Multiple credentials are saved`. The hint lists them; select one with `--account NAME`, or save a profile with `voltage profiles create`.

### `unknown_credential`

`error: Unknown credential NAME`. The hint lists the saved names. `voltage auth status --account NAME` shows one.

### `empty_api_key`

`error: VOLTAGE_API_KEY is empty`. A blank variable still hides saved credentials. Unset it, or set it to a key.

### `bound_elsewhere`

An imported API key belongs to one organization and environment, and the command selected another. Select a credential for that scope with `--account NAME`, or use a browser login.

### `auth_url_mismatch`

`error: Saved credentials cannot be refreshed at a different auth URL`. A browser login refreshes only at the auth URL it was saved with. Omit `--auth-url` to use that URL, which `voltage context` shows. To move the login to the new URL, run `voltage logout --account NAME`, then `voltage login --account NAME --auth-url URL`.

### `login_expired`

`error: The saved login for NAME has expired or was revoked`. The auth service rejected the login's refresh or token exchange with `invalid_grant`. `--json` reports `"credential_problem": "login_expired"` in `error.detail`. Sign in again with the command in the hint:

```sh
voltage login --account NAME
```

When you sign in as the same user, the new login replaces the saved one, and the CLI revokes the replaced session. Keep the same `NAME` so profiles that select it keep working. If you sign in as a different user, `voltage login` refuses the name; run `voltage logout --local --account NAME` first, or choose another name. It refuses the name the same way when the saved login's secret cannot be read, for example from a locked OS credential store or a damaged credential file. `voltage logout --local --account NAME` removes that account even when its secret cannot be removed, and warns you to remove the secret yourself.

`voltage context` cannot report this problem. It reads no saved secret and makes no network request, so it cannot see that the auth service rejects the login. An expired access token alone is not this problem, because the next command refreshes it. `voltage auth status` shows whether the access token has expired, and the next command that uses the login reports `login_expired`.

### needs a browser login

`organizations list`, `environments list`, and `environments create` need a browser login; an API key cannot run them. Pass `--account NAME` for a saved login, or unset `VOLTAGE_API_KEY`.

### HTTP 401

The API did not accept the credential. `voltage auth status` shows the selected one and whether it has expired. For a saved login, replace it as described in [`login_expired`](#login_expired).

A checkout command presents its session or stream token, not the account credential. Check the token in `--token-file`, `VOLTAGE_CHECKOUT_TOKEN`, or `VOLTAGE_STREAM_TOKEN`.

### HTTP 403

The credential is valid but cannot reach the selected organization or environment. `voltage context` shows which scope and credential the command used.

When the command used `VOLTAGE_API_KEY` instead of a saved login, the hint says so, and names where the organization came from:

```text
hint: The API key came from VOLTAGE_API_KEY and the organization from VOLTAGE_ORGANIZATION_ID. Check that the key can reach the organization, environment, and wallet in use, which `voltage context` shows, or pass --account NAME or unset VOLTAGE_API_KEY to use a saved login.
```

With `--json`, `error.detail.credential_sources` holds the same sources, as `{"credential": "environment", "organization": "environment"}`. The credential is always `environment`. The organization is `environment` for `VOLTAGE_ORGANIZATION_ID`, `flag` for `--org`, and `unset` when neither supplies it.

### Cannot read OS credential store

The keychain is locked or missing, as on headless Linux without an unlocked Secret Service. Unlock it, or log in again with `--credential-store file`. See [credential stores](../reference/configuration.md#credential-stores).

## API features

### `feature_flag_disabled`

On-chain and BIP21 payments and treasury movements are enabled per organization. Ask Voltage to enable the feature for yours.

### `quote_required`

A wallet on a USD line of credit needs a quote for every payment. Create a quote with `voltage quotes create`, then pass `--quote QUOTE_ID` and the same amount you quoted to the payment: the invoice's BTC amount to pay, or the USD amount to receive. See [quotes and lines of credit](payments.md#quotes-and-lines-of-credit).

The CLI refuses a USD payment without `--quote` itself, with `A USD payment needs --quote QUOTE_ID`, before it sends anything. A wallet on a BTC line of credit cannot pay or receive in USD; use `--currency btc`.

### `invalid_currency` with a quoted amount

When the detail names a quoted amount, the quote does not convert to exactly the payment's amount. Check what it converts to with `voltage quotes get QUOTE_ID`. To pay a BTC invoice from a USD wallet, quote the invoice's BTC amount with `--unit sats --to usd`. See [quotes and lines of credit](payments.md#quotes-and-lines-of-credit).

Related: [exit codes](../reference/exit-codes.md), [environment variables](../reference/environment.md), [log in and use profiles](login-and-profiles.md).
