# Configuration

The configuration directory is `--config-dir`, else `VOLTAGE_CONFIG_DIR`, else `$XDG_CONFIG_HOME/voltage`, else `~/.config/voltage`. It must be an absolute path. The CLI creates it owner-only (mode `0700` on Unix) and refuses one that is a symlink or owned by another user. `voltage context` shows which directory is in use and creates nothing.

| Path | Contents |
|---|---|
| `config.toml` | Saved credentials' non-secret facts (name, kind, store, email, bound organization and environment, auth URL) and profiles. No secrets. |
| `credentials/` | Secrets for credentials saved with `--credential-store file`, one owner-only file each |
| `credentials.lock` | Serializes login refreshes and credential changes across concurrent commands |
| `requests/` | The request journal: each change that carries an ID (a payment, a treasury movement, `wallets create --id`, or a `--data` body with an `id`), with its operation, scope, and request hash, never the body. See [payment IDs](../concepts/safety-model.md#payment-ids-and-the-request-journal). |

## Credential stores

A credential's secret is kept in one store, chosen when you log in or import a key:

- `keychain` (default): the macOS Keychain, the Linux Secret Service, or the Windows Credential Manager, under the service name `voltage-cli`.
- `file`: an owner-only file under `credentials/`, for machines without an unlocked keychain, such as headless Linux.

## Profiles

A profile saves an organization, an environment, and a credential under one name. `--profile NAME` uses it as a complete configuration; see [log in and use profiles](../guides/login-and-profiles.md).

Related: [environment variables](environment.md), [global flags](global-flags.md), [install](../guides/install.md#credential-stores).
