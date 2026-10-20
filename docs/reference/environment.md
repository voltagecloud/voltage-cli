# Environment variables

A flag beats a variable. A selected profile is a complete configuration, so `VOLTAGE_ORGANIZATION_ID`, `VOLTAGE_ENVIRONMENT_ID`, `VOLTAGE_WALLET_ID`, and `VOLTAGE_API_KEY` are ignored while one is selected; `--account` alone ignores only `VOLTAGE_API_KEY`. `voltage context` lists every variable that is set but ignored, and where each value it uses came from.

| Variable | Used for | Flag that wins |
|---|---|---|
| `VOLTAGE_API_KEY` | An environment API key, used when no profile or account is selected. A blank value still shadows saved credentials, so commands fail instead of falling back. | `--profile`, `--account` |
| `VOLTAGE_ORGANIZATION_ID` | Organization UUID | `--org`, `--profile` |
| `VOLTAGE_ENVIRONMENT_ID` | Environment UUID | `--env`, `--profile` |
| `VOLTAGE_WALLET_ID` | Wallet UUID | `--wallet`, `--profile` |
| `VOLTAGE_EXECUTE` | `1` or `true` sends every change without `--execute`; `0`, `false`, or empty only describes changes. Any other value fails before a change. | `--execute` |
| `VOLTAGE_CHECKOUT_TOKEN` | A checkout session token for `checkout sessions` reads | `--token-file` |
| `VOLTAGE_STREAM_TOKEN` | A checkout stream token for `checkout events watch` | `--token-file` |
| `VOLTAGE_CONFIG_DIR` | The configuration directory, as an absolute path | `--config-dir` |
| `XDG_CONFIG_HOME` | Parent of the default configuration directory, `$XDG_CONFIG_HOME/voltage` (else `~/.config/voltage`) | `--config-dir`, `VOLTAGE_CONFIG_DIR` |
| `COLUMNS` | Terminal width for human tables; otherwise the terminal's own width | — |

Related: [configuration](configuration.md), [global flags](global-flags.md), [scripting](../guides/scripting.md).
