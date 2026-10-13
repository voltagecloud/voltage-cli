# Requests

Commands mirror the API contract; see [the command reference](../commands.md) for the full list with routes. Every command and group has descriptive `--help`; body-building commands also show required flag combinations and examples:

```sh
voltage --help
voltage payments --help
voltage payments receive --help
```

## Request bodies

Every operation that takes a body accepts its complete API payload from a file or stdin. The JSON is sent exactly as given, including fields the CLI does not know about, and is limited to 16 MiB:

```sh
voltage payments create --profile prod --data @payment.json --execute
cat payment.json | voltage payments create --profile prod --data - --execute
```

Common operations also offer friendly flags (`wallets create`, `wallets update`, `payments receive`, `payments send`, `quotes create`, `webhooks create`, `webhooks update`); `--help` lists them. Friendly flags and `--data` are mutually exclusive, and a payload whose IDs conflict with the selected scope is rejected.

## Filters

Documented query filters are flags named after the parameter. Parameters with a fixed set of values accept only those values, case-insensitively, and are sent in the contract's spelling. `--query NAME=VALUE` sets any documented parameter and can be repeated:

```sh
voltage payments list --profile prod --statuses completed --statuses failed \
  --metadata order_id=123 --query sort_order=desc --all
```

## Paging and timeouts

`--all` follows pagination by cursor, which the API recommends and the CLI requests by default; endpoints that only page by offset are followed by offset and count. The API has deprecated explicit offset paging (`--offset`, `--pagination offset`) where cursors exist, and the CLI warns when you use it. `--timeout SECONDS` (default 60) bounds the whole request, including paging and waits.

Related: [safety model](../concepts/safety-model.md), [output reference](../reference/output.md).
