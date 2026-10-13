# Changelog

User-visible changes to `voltage`, newest first. Each pull request that changes what users see adds a line under **Unreleased**; see [AGENTS.md](AGENTS.md).

## Unreleased

### Added

- Pick a missing organization, environment, or wallet from a list on a terminal.
- `voltage context` shows the effective profile, scope, credential, API and auth URLs, and where each came from.
- `--dry-run` describes any API request offline; changes are only described unless `--execute` or `VOLTAGE_EXECUTE=1` asks to send them.
- `--no-input` and `--quiet`, with interrupt-safe progress and the time waited so far on payment waits and device approval.
- `voltage --version` shows the git build.
- Every command that changes something has examples in `--help`, and every command links its guide.
- Guides for wallets, webhooks, troubleshooting, and getting started, and references for global flags, environment variables, and configuration.

### Changed

- Human tables fit the terminal width, and all API text is scrubbed of terminal control characters.
- Each item of a string list gets its own line in human output.
- Errors name the next command: missing scope names the command that lists IDs, credential and profile errors list the saved names, and HTTP 401 and 403 point to `voltage auth status` and `voltage context`.
- `-h` shows usage and options; `--help` adds notes, examples, and the guide link.

### Fixed

- Help and README examples for changes include `--execute`, so copying one sends it.

## 0.1.0 - 2026-09-18

First release: a command for every Voltage API operation, browser login and API keys, profiles, payment waits with a request journal, hosted checkout, prices, and signed release artifacts.
