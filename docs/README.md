# Voltage CLI documentation

Start with the [README](../README.md) for install and a quick start.

## Do a task

- [Install](guides/install.md): platforms, download verification, building from source, credential stores, completions
- [Log in and use profiles](guides/login-and-profiles.md): browser login, API keys, scope, profiles, `voltage context`
- [Payments](guides/payments.md): receive, send, wait, invoices, quotes, prices
- [Requests](guides/requests.md): request bodies, friendly flags, filters, paging
- [Checkout](guides/checkout.md): session and stream tokens, events
- [Scripting](guides/scripting.md): JSON, `VOLTAGE_EXECUTE`, running without a person

## Look it up

- [Command reference](commands.md): every command and its API route
- [Output](reference/output.md): tables, the JSON envelope, stdout and stderr
- [Exit codes](reference/exit-codes.md)

## Understand

- [Safety model](concepts/safety-model.md): `--execute`, dry runs, payment IDs, Ctrl-C, redaction

## For AI agents

Use `--json` and branch on exit codes. Describe a change with `--dry-run` before you send it with `--execute`. Never pass `--show-secrets`. For product concepts, see the [Voltage API docs](https://voltageapi.com/v1/docs).

## For contributors

- [Development guide](development.md): build, test, release
- [Style guide](quality.md): code conventions
