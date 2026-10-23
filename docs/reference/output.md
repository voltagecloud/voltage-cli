# Output reference

On a terminal, results are readable tables; when piped, they are JSON. `--json` forces JSON and `--output table|json|ndjson` selects explicitly.

## Tables

- Tables show up to seven of the fields a list's items share (ID, name, status, amount, and similar) with aligned columns, a result count, and the next cursor.
- Amounts are shown with grouped digits: every BTC amount in sats, with up to three decimals for msats (`150,000 sats`, `1.5 sats`), so a column never mixes units, and cents as dollars (`$1,234.56`). JSON keeps the API's integer and base unit.
- A table wider than the terminal (its width, or `COLUMNS`) hides trailing columns and then shortens text cells with `…`, naming what it hid. IDs, timestamps, and amounts are never shortened.
- Table timestamps are relative to now, such as `3 minutes ago` or `in 2 days`; anything within a minute either way reads `just now`. A single resource shows the UTC time to the second with the relative time beside it, such as `2024-02-27 01:58:51 UTC (7 months ago)`. JSON keeps the exact value.
- With color on, table headers and field names are bold, a known status is colored by meaning (`completed` green, `failed` red, `sending` yellow), and so is the outcome line (`accepted`, `dry_run`). Color is added after layout, so it never moves a column; an unknown status stays plain.
- A table starts with the outcome only when it adds something, such as `accepted` or `dry_run`, or when there is no data to show.
- A single resource is shown as aligned `field  value` lines, with nested fields joined by dots. `voltage context` shows each setting as `name  value  (source)`.
- On a terminal, `voltage context` and `voltage auth status` draw those lines inside a titled card (rounded corners, or `+-|` without a UTF-8 locale). A card wider than the terminal falls back to the plain lines, and redirected output always gets them.

The table layout may change between releases; scripts should use JSON.

## JSON

The JSON envelope is stable:

```json
{"http_status":202,"data":null,"resource_id":"PAYMENT_ID","outcome":"accepted"}
```

API fields stay inside `data`. With `--all`, JSON output collects the pages into `data.pages`, and NDJSON writes one envelope per page as it arrives.

## Streams

Diagnostics, prompts, and progress go to stderr. Interactive stderr shows an animated spinner for network requests and waits, with the time waited so far on payment waits and device approval and on any wait past two seconds, and a countdown to when login stops waiting for approval (the code's expiry, or 10 minutes at most); it is cut to the terminal's width and cleared when the wait ends or is cancelled. `VOLTAGE_SPINNER=0` (or `false`) or `TERM=dumb` draws it as a still line; a Windows console without ANSI support shows none. Redirected stderr gets no spinner or control codes.

On interactive stderr a payment wait reads as a timeline: `› Payment ID accepted; waiting for completed`, one `› status · time` line each time the status changes, and a closing `⚡ Payment ID completed · 4s` (`✓ Payment ID ready` for an invoice that is ready but unpaid) or `✗ Payment ID failed · 4s` before the error. The closing line comes after the payment's record, so the outcome is the last thing on screen; for a ready invoice with `--qr`, the code follows it. Redirected stderr keeps the plain `Payment ID status: sending; waiting for completed.` lines, repeated every 15 seconds while the status holds. Symbols are Unicode when the locale is UTF-8 and `TERM` is not `dumb` or `linux` (on Windows, in Windows Terminal) and ASCII otherwise (`ok` for `✓` and `⚡`, `x`, `>`). `-q, --quiet` suppresses optional status and notices, never requested results, errors, warnings, or recovery IDs.

Human output and help are colored on a terminal: `error:` is red, `hint:` cyan, and `details:` dim, and color only marks a label, so the words read the same without it. `--color always|never` (or `--no-color`) decides; otherwise `NO_COLOR`, `FORCE_COLOR`, `CLICOLOR_FORCE`, `CLICOLOR`, `TERM=dumb`, and whether each stream is a terminal decide, in that order, separately for stdout and stderr. Help headers and usage are Voltage orange: 24-bit with `COLORTERM=truecolor`, 256-color on a `256color` terminal, and bold elsewhere; every other color is one of the 16 basic colors your terminal theme sets. JSON, NDJSON, `--output-file`, and completions are never colored, even with `--color always`.

Human errors start with `error:`, which leads with the API's reason when it rejects a request with a client error (HTTP 4xx), and may include a safe `hint:` and an indented `details:` block of `field  value` lines. `--json` reports the complete API response in `error.detail`, and makes command-line parse errors machine-readable as well as runtime errors. A downstream consumer that closes stdout early is treated as successful pipeline completion.

Related: [scripting](../guides/scripting.md), [safety model](../concepts/safety-model.md#secrets-and-redaction).
