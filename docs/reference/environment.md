# Environment variables

A flag beats a variable. A selected profile is a complete configuration, so `VOLTAGE_ORGANIZATION_ID`, `VOLTAGE_ENVIRONMENT_ID`, `VOLTAGE_WALLET_ID`, and `VOLTAGE_API_KEY` are ignored while one is selected; `--account` alone ignores only `VOLTAGE_API_KEY`. Every command that reads the configuration prints one `warning:` line on stderr for each variable that is set but ignored, before it prompts or sends a request, even with `--quiet`; stdout is unchanged. `auth import-key` and `profiles create` save the scope and account their flags name, so they warn only about a profile or `--config-dir`. `voltage context` lists the same variables, and where each value it uses came from.

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
| `VOLTAGE_SPINNER` | `0` or `false` draws progress as a still line instead of an animated spinner, for screen readers, which read every redraw | — |
| `NO_COLOR` | Any non-empty value turns off color in human output and help | `--color` |
| `FORCE_COLOR` | `0` or `false` turns off color; any other non-empty value colors human output and help even when redirected. `2` and `3` also select 256 and 24-bit color for the brand color | `--color`, `NO_COLOR` |
| `COLORTERM` | `truecolor` or `24bit` draws the brand color in 24-bit color; otherwise a `TERM` naming `256color` draws it in 256 colors, and other terminals show it as bold | — |
| `CLICOLOR_FORCE` | Any non-zero value colors human output and help even when redirected | `--color`, `NO_COLOR`, `FORCE_COLOR` |
| `CLICOLOR` | `0` turns off color on a terminal | `--color`, `NO_COLOR`, `CLICOLOR_FORCE` |

Related: [configuration](configuration.md), [global flags](global-flags.md), [scripting](../guides/scripting.md).
