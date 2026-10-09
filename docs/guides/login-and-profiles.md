# Log in and use profiles

## Log in

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

Resource IDs are positional. The enclosing scope comes from `--org`, `--env`, `--wallet`, and `--webhook`:

```sh
voltage wallets list --org ORG_ID --env ENV_ID
voltage wallets get WALLET_ID --org ORG_ID
voltage payments list --org ORG_ID --env ENV_ID --env OTHER_ENV_ID
```

Without a profile, flags override `VOLTAGE_ORGANIZATION_ID`, `VOLTAGE_ENVIRONMENT_ID`, and `VOLTAGE_WALLET_ID`.

Only endpoints that filter by environment receive `--env`. Organization-wide commands say so in their help, and wallet mutations verify a supplied environment against the wallet before changing anything.

## Pick the scope interactively

On a terminal, a command that lacks the organization, environment, or wallet it needs asks for it instead of failing. The picker lists each choice as `name  UUID`; type to filter, use the arrow keys, and press Enter. Afterwards, the CLI prints the flags that select the same scope, so you can pass them next time or save a profile.

```sh
voltage wallets list                            # asks for the organization
voltage payments receive --currency btc --kind bolt11 --amount 1000 --unit sats --execute
```

An API key cannot list organizations or environments, so with a key the CLI offers only wallets; pass `--org` and `--env` or use a profile. Esc leaves the scope unchanged, and the command then reports what is missing. Ctrl-C exits with code 130. Picking a scope never sends a change: changes still need `--execute`. The CLI never asks when stdin or stderr is not a terminal, or with `--no-input`, `--json`, `--output json|ndjson`, or `--dry-run`. In those cases, the command fails with the same error as before.

## Profiles

A profile bundles an organization, an environment, and a credential, and ignores the scope variables above and `VOLTAGE_API_KEY`; flags still override its scope for one command.

```sh
voltage profiles create staging --org ORG_ID --env ENV_ID --account work
voltage profiles list
voltage profiles get staging
voltage profiles delete staging
```

## See the effective scope

`voltage context` shows the effective profile, account, organization, environments, wallet, API URL, and configuration directory, and where each came from (`flag`, `profile`, `environment`, `default`, or `unset`). It reports why commands would reject the credential selection as `credential_problem` (`empty_api_key`, `no_credential`, `multiple_credentials`, `unknown_credential`, or `bound_elsewhere` for an API key used outside its organization or environment), using the same selection commands use. It also lists `VOLTAGE_*` variables that are set but ignored because a flag or the selected profile takes precedence. It reads no saved secret, makes no network request, and creates no files:

```sh
voltage context --profile staging
voltage context --json | jq -r '.data.organization_id.value'
```

## Configuration directory

Configuration lives in `$XDG_CONFIG_HOME/voltage` or `~/.config/voltage`; override the directory with `VOLTAGE_CONFIG_DIR` or `--config-dir`. The directory and everything in it are owner-only.

Related: [install](install.md), [payments](payments.md), [scripting](scripting.md).
