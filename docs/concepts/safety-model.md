# Safety model

`voltage` moves money. You see every change before it is sent. You can trace every payment after a failure. Secrets stay out of output.

## Changes need `--execute`

Commands that change something (any method but GET) only describe their request unless you pass `--execute`, then exit with code 6. Reads run as usual. `--execute` is the only approval: nothing asks again before sending. A described change reports the same `resource_id` a real run would use, so chained commands see the same IDs in both modes. Scripts can set `VOLTAGE_EXECUTE=1` instead; see [scripting](../guides/scripting.md#send-changes-from-a-script).

## Dry runs

`-n, --dry-run` validates any API command and describes the request it would send, then stops: it reports the method, URL, query, `Origin` header, and body, plus a `run` object with what a real run would do next. That is the same plan a real run follows:

- A `read` says whether it checks the wallet's environment and whether it then pages, waits for a payment, or waits for a checkout session.
- A `mutation` says whether it verifies the wallet's environment, the ID it would journal for recovery, whether a failed payment submission is reconciled, and what it waits for.
- An `event_stream` follows checkout events.

A dry run authenticates nothing, makes no network request (including the wallet environment check a real mutation performs), prompts for nothing, writes no journal entry, and does not create `--output-file`. Body and metadata-filter values are replaced with `[OMITTED]` unless the field holds only identifiers, enums, or amounts; `--show-secrets` shows them. `--data -` still reads stdin, and a friendly create generates a fresh ID each run unless you pass `--id`. Passing local validation does not mean the API will accept the request.

```sh
voltage payments send --profile prod --wallet WALLET_ID --currency btc \
  --invoice BOLT11_INVOICE --max-fee 10 --fee-unit sats --dry-run
voltage wallets delete WALLET_ID --profile prod -n --json
```

The result is a normal envelope with `outcome: dry_run` and no `http_status`.

## Payment IDs and the request journal

Payments and treasury movements carry an ID, generated for you unless you pass `--id`. Before sending, the CLI records the ID, operation, scope, and request hash (never the body) in a private journal under `requests/`, and it refuses to reuse an ID with a different request. If the connection drops after a submission, the CLI makes one short read of the ID: if the payment is visible, it reports acceptance; otherwise it exits with code 4 and the ID, without resubmitting. Check the ID before deciding to retry.

## Ctrl-C

Ctrl-C exits with code 130, including at the hidden API-key prompt. Before a write is sent, it reports that no resource change was submitted, or, if a login was being refreshed, created, or logged out, how to recover it. Once a write may have been sent, it reports the outcome as unknown, with the original ID and reconciliation instructions for a payment. It never cancels a submitted request.

## Secrets and redaction

Known secret fields and the credentials the CLI presented are redacted from normal output and from error details. Operations that return a one-time secret (webhook creation and key rotation, checkout sessions, stream tokens) refuse to run unless you pass `--output-file PATH`, which writes the complete response to a new owner-only file, or `--show-secrets`. Treat your own metadata as potentially sensitive; redaction cannot recognise it.

API text is stripped of terminal control, bidirectional formatting, and invisible characters (including tag characters that can carry hidden text) in tables, notices, and errors.

Related: [payments](../guides/payments.md), [exit codes](../reference/exit-codes.md).
