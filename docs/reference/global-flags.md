# Global flags

Generated from the command definitions by `VOLTAGE_BLESS=1 cargo test`; do not edit by hand. Every command accepts these flags, before or after its name.

## Scope

| Flag | Meaning |
|---|---|
| `--profile PROFILE` | Use this named profile as a complete configuration |
| `--account ACCOUNT` | Select a saved credential |
| `--org UUID` | Organization UUID |
| `--env UUID` | Environment UUID (repeat for supported list filters) |
| `--wallet UUID` | Wallet UUID or wallet filter |
| `--webhook UUID` | Parent webhook UUID |

## Output

| Flag | Meaning |
|---|---|
| `--json` | Write a stable JSON result envelope |
| `--output OUTPUT` | Output format (`table`, `json`, `ndjson`) |
| `--output-file PATH` | Save the complete response to a new owner-only file |
| `--show-secrets` | Explicitly allow secrets in result output |
| `-q`, `--quiet` | Hide optional progress and notices, not errors, results, or recovery IDs |

## Safety

| Flag | Meaning |
|---|---|
| `--no-input` | Never prompt for a secret; supply it with --stdin or the environment instead |

## Request

| Flag | Meaning |
|---|---|
| `--timeout SECONDS` | HTTP or wait deadline in seconds; default `60` |

## Configuration

| Flag | Meaning |
|---|---|
| `--config-dir DIR` | Configuration directory (default: ~/.config/voltage) |
| `--api-url URL` | Explicit API base URL; never persisted |
| `--auth-url URL` | Explicit auth base URL; never persisted |
| `--price-url URL` | Explicit price service base URL; never persisted |

Related: [environment variables](environment.md), [configuration](configuration.md), [output](output.md).
