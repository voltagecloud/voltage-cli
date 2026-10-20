# Output reference

On a terminal, results are readable tables; when piped, they are JSON. `--json` forces JSON and `--output table|json|ndjson` selects explicitly.

## Tables

- Tables show up to seven of the fields a list's items share (ID, name, status, amount, and similar) with aligned columns, a result count, and the next cursor.
- Amounts are shown in their base units (`msats` or `cents`).
- A table wider than the terminal (its width, or `COLUMNS`) hides trailing columns and then shortens text cells with `…`, naming what it hid. IDs, timestamps, and amounts are never shortened.
- Timestamps are shown in UTC to the second, such as `2024-02-27 01:58:51 UTC`; JSON keeps the exact value.
- A table starts with the outcome only when it adds something, such as `accepted` or `dry_run`, or when there is no data to show.
- A single resource is shown as aligned `field  value` lines, with nested fields joined by dots. `voltage context` shows each setting as `name  value  (source)`.

The table layout may change between releases; scripts should use JSON.

## JSON

The JSON envelope is stable:

```json
{"http_status":202,"data":null,"resource_id":"PAYMENT_ID","outcome":"accepted"}
```

API fields stay inside `data`. With `--all`, JSON output collects the pages into `data.pages`, and NDJSON writes one envelope per page as it arrives.

## Streams

Diagnostics, prompts, and progress go to stderr. Interactive stderr shows immediate, cancellable status for network requests and waits; redirected stderr gets no spinner/control codes. `-q, --quiet` suppresses optional status and notices, never requested results, errors, warnings, or recovery IDs.

Human errors start with `error:` and may include a safe `hint:`. `--json` makes command-line parse errors machine-readable as well as runtime errors. A downstream consumer that closes stdout early is treated as successful pipeline completion.

Related: [scripting](../guides/scripting.md), [safety model](../concepts/safety-model.md#secrets-and-redaction).
