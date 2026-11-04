# Log in and use profiles

## Log in

`voltage login` prints a verification URL and a code, opens the URL in your browser, and waits for your approval after your usual login and MFA. On a terminal the URL is a clickable link (in terminals that support links) whose text is the whole URL, the code is bold, the wait counts down to when the CLI stops waiting for approval (the code's expiry, or 10 minutes at most), and a closing `✓ Logged in as EMAIL` line names who signed in, under the `voltage` wordmark, a yellow lightning bolt beside the name in Voltage orange (left out on a terminal narrower than 40 columns, and with `--quiet`). Use `--no-browser` on a remote machine and open the URL yourself. The saved login refreshes its tokens automatically; the refresh token rotates.

```sh
voltage login                                   # saved under your email address
voltage login --account work                    # saved under a name of your choice
voltage login --no-browser --credential-store file
voltage auth status                             # identity and expiry, never the secret
voltage logout                                  # revokes the session, then removes it
voltage logout --local                          # removes local credentials only
```

A login grants the permissions your account already has. Profiles do not restrict them.

On a terminal, a login then offers to save a [profile](#profiles) pinned to an organization and environment, so later commands need only `--profile NAME`. It takes the organization and the environment when there is only one of each, and otherwise asks, as the [picker](#pick-the-scope-interactively) does. Then it asks for the profile's name (`default` when you press Enter). Esc skips the profile. When a profile already uses this login, or when stdout is not a terminal, as with `--json`, or with `--no-input`, nothing is offered. A failure while offering, such as a name that is already used, leaves the login saved and says how to create the profile later with `voltage profiles create`. Ctrl-C exits with code 130 and says that the login is saved. The profile is saved under the same lock as `voltage profiles create`, after the configuration is read again, so a change another command made while the prompt was open is kept. With table output, the result shows `profile` when one was saved.

Signing in again under a saved name replaces that login when it is the same user at the same auth URL, and revokes the replaced session. A different user, or a name used by an imported API key, is refused. With `--account NAME`, an API key or a login at another auth URL is refused before the browser step. Without `--credential-store`, the new login stays in the replaced login's credential store; a new name uses the keychain. With `--json`, the output has `replaced: true` when the login replaced a saved one.

## API keys

For automation, use an environment API key. Import one so it never lands in shell history, or pass it through `VOLTAGE_API_KEY`:

```sh
voltage auth import-key --account ci --org ORG_ID --env ENV_ID
secret-manager-command | voltage auth import-key --stdin --account ci --org ORG_ID --env ENV_ID
VOLTAGE_API_KEY=... voltage wallets list --org ORG_ID
```

An imported key remembers the organization and environment it belongs to, and a command that selects a different one fails before any request is sent. `VOLTAGE_API_KEY` is used only when neither `--profile` nor `--account` is given. `organizations list` and `environments list` require a browser login.

With several saved credentials, select one with `--account` or through a profile.

## Scope

A resource ID is positional, or passed as `--id ID`, as in `voltage wallets get --id WALLET_ID`. A command that acts on one wallet also takes it as `--wallet WALLET_ID`; a profile's or `VOLTAGE_WALLET_ID`'s wallet never selects it. The enclosing scope comes from `--org`, `--env`, `--wallet`, and `--webhook`:

```sh
voltage wallets list --org ORG_ID --env ENV_ID
voltage wallets get WALLET_ID --org ORG_ID
voltage payments list --org ORG_ID --env ENV_ID --env OTHER_ENV_ID
```

Without a profile, flags override `VOLTAGE_ORGANIZATION_ID`, `VOLTAGE_ENVIRONMENT_ID`, and `VOLTAGE_WALLET_ID`.

Only endpoints that filter by environment receive `--env`. Organization-wide commands say so in their help, and wallet mutations verify a supplied environment against the wallet before changing anything.

## Pick the scope interactively

On a terminal, a command that lacks the organization, environment, or wallet it needs asks for it instead of failing. The picker lists each choice as `name  UUID`, and each wallet as `name  CURRENCY  UUID` when all of its balances are in one currency. Type to filter, by name or currency, use the arrow keys, and press Enter. When there is only one organization or environment, the CLI takes it without asking and says so. A wallet is always asked for, also when there is only one, because a payment spends from it. Afterwards, the CLI prints the flags that select the same scope, so you can pass them next time or save a profile.

```sh
voltage wallets list                            # asks for the organization
voltage payments receive --currency btc --kind bolt11 --amount 1000 --unit sats --execute   # asks for the wallet
```

The picker offers every wallet in the environment and shows a wallet's currency when its balances report one. A wallet with no balance yet shows no currency. For this example, pick a BTC wallet. Typing filters on the whole label, so `btc` matches wallets that show `BTC` and any wallet with `btc` in its name. A wallet on a USD line of credit needs a quote for every payment, so the API rejects the receive with `quote_required` unless you pass `--quote QUOTE_ID` from `voltage quotes create`.

An API key cannot list organizations or environments, so with a key the CLI offers only wallets; pass `--org` and `--env` or use a profile. Esc leaves the scope unchanged, and the command then reports what is missing. Ctrl-C exits with code 130. Picking a scope never sends a change: changes still need `--execute`. The CLI never asks when stdin or stderr is not a terminal, when stdout is piped without `--output table` (as in `voltage wallets list | jq`), or with `--no-input`, `--json`, or `--output json|ndjson`. In those cases, the command fails with the same error as before.

## Profiles

A profile bundles an organization, an environment, and a credential, and ignores the scope variables above and `VOLTAGE_API_KEY`; flags still override its scope for one command. Other tools, such as the `voltage-api` agent skill's helper, read the same variables and let them win, so each command names on stderr every variable it ignores, even with `--quiet`:

```console
$ VOLTAGE_WALLET_ID=WALLET_ID voltage wallets list --profile staging
warning: VOLTAGE_WALLET_ID is set but ignored because --profile staging is selected
```

```sh
voltage profiles create staging --org ORG_ID --env ENV_ID --account work
voltage profiles list
voltage profiles get staging
voltage profiles delete staging
```

## See the effective scope

`voltage context` shows the effective profile, account, organization, environments, wallet, API and auth URLs, and configuration directory, and where each came from (`flag`, `profile`, `environment`, `default`, or `unset`). It reports why commands would reject the credential selection as `credential_problem` (`empty_api_key`, `no_credential`, `multiple_credentials`, `unknown_credential`, `bound_elsewhere` for an API key used outside its organization or environment, or `auth_url_mismatch` for a browser login used with a different `--auth-url`), using the same selection commands use. It cannot report `login_expired`, a login the auth service no longer accepts: that needs a network request, so the next command that uses the login reports it in `error.detail.credential_problem`. It also lists `VOLTAGE_*` variables that are set but ignored because a flag or the selected profile takes precedence. It reads no saved secret, makes no network request, and creates no files:

```sh
voltage context --profile staging
voltage context --json | jq -r '.data.organization_id.value'
```

## Configuration directory

Configuration lives in `$XDG_CONFIG_HOME/voltage` or `~/.config/voltage`; override the directory with `VOLTAGE_CONFIG_DIR` or `--config-dir`. The directory and everything in it are owner-only.

Related: [install](install.md), [payments](payments.md), [scripting](scripting.md).
