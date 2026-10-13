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

The connection failed. For a payment or treasury movement, the request may have been sent: the error reports the ID, and the CLI never resubmits it. Read it with `voltage payments get PAYMENT_ID` before you decide to retry. See [payment IDs](../concepts/safety-model.md#payment-ids-and-the-request-journal).

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

### The saved login has expired or was revoked

Sign in again with the command in the hint, `voltage login --account NAME`.

### needs a browser login

`organizations list` and `environments list` need a browser login; an API key cannot list them. Pass `--account NAME` for a saved login, or unset `VOLTAGE_API_KEY`.

### HTTP 401

The API did not accept the credential. `voltage auth status` shows the selected one and whether it has expired; sign in again with `voltage login`.

### HTTP 403

The credential is valid but cannot reach the selected organization or environment. `voltage context` shows which scope and credential the command used.

### Cannot read OS credential store

The keychain is locked or missing, as on headless Linux without an unlocked Secret Service. Unlock it, or log in again with `--credential-store file`. See [credential stores](../reference/configuration.md#credential-stores).

## API features

### `feature_flag_disabled`

On-chain and BIP21 payments and treasury movements are enabled per organization. Ask Voltage to enable the feature for yours.

### `quote_required`

The wallet is on a USD line of credit, which needs a quote for every payment. Create a quote with `voltage quotes create`, then pass `--quote QUOTE_ID` and the same USD amount to the payment. See [quotes and lines of credit](payments.md#quotes-and-lines-of-credit).

Related: [exit codes](../reference/exit-codes.md), [environment variables](../reference/environment.md), [log in and use profiles](login-and-profiles.md).
