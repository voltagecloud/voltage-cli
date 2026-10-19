# Safety model

`voltage` moves money. You see every change before it is sent. You can trace every payment after a failure. Secrets stay out of output.

## Changes need `--execute`

Commands that change something (any method but GET) only describe their request unless you pass `--execute`. Reads run as usual. `--execute` is the only approval: nothing asks again before sending.

| Command | What happens | Exit code |
|---|---|---|
| A read (`list`, `get`, `watch`) | Sends the request | 0 on success |
| A change without `--execute` | Describes the request and sends nothing | 6 |
| A change with `--execute` | Sends the request | 0 on success |

Exit code 6 means "described, not sent", so a script that forgot `--execute` fails instead of looking as if it sent money. To check a change on purpose, run it without `--execute` and treat exit code 6 as the expected result. Scripts can set `VOLTAGE_EXECUTE=1` instead of passing `--execute`, and `VOLTAGE_EXECUTE=0` describes one command again; see [scripting](../guides/scripting.md#send-changes-from-a-script).

```sh
voltage payments send --profile prod --wallet WALLET_ID --currency btc \
  --invoice BOLT11_INVOICE --max-fee 10 --fee-unit sats
voltage wallets delete WALLET_ID --profile prod
```

## What a described change shows

The description reports the method, URL, query, `Origin` header, and body, plus a `run` object with what a real run would do next: whether it verifies the wallet's environment, the ID it would journal for recovery, whether a failed payment submission is reconciled, and what it waits for. It reports the same `resource_id` a real run would use, so chained commands see the same IDs in both modes. The result is a normal envelope with `outcome: dry_run` and no `http_status`.

Describing a change makes no request for the change itself (including the wallet environment check a real run performs), writes no journal entry, and does not create `--output-file`. It authenticates nothing, with one exception: a payment without `--currency` reads its wallet's currency, as a real run would, so the description shows the same body; pass `--currency` to keep it offline. On a terminal, a missing organization, environment, or wallet can still open a [picker](../guides/login-and-profiles.md#pick-the-scope-interactively), which lists choices from the API; pass the scope, `--no-input`, or `--json` to stay offline. Body and metadata-filter values are replaced with `[OMITTED]` unless the field holds only identifiers, enums, or amounts; `--show-secrets` shows them. `--data -` still reads stdin, and a friendly create generates a fresh ID each run unless you pass `--id`. Passing local validation does not mean the API will accept the request.

## Payment IDs and the request journal

Payments and treasury movements carry an ID, generated for you unless you pass `--id`. Before sending, the CLI records the ID, operation, scope, and request hash (never the body) in a private journal under `requests/`, and it refuses to reuse an ID with a different request. If the connection drops after a submission, the CLI makes one short read of the ID: if the payment is visible, it reports acceptance; otherwise it exits with code 4 and the ID, without resubmitting. Check the ID before deciding to retry.

## Ctrl-C

Ctrl-C exits with code 130, including at the hidden API-key prompt. Before a write is sent, it reports that no resource change was submitted, or, if a login was being refreshed, created, or logged out, how to recover it. Once a write may have been sent, it reports the outcome as unknown, with the original ID and reconciliation instructions for a payment. It never cancels a submitted request.

## Secrets and redaction

Known secret fields and the credentials the CLI presented are redacted from normal output and from error details. Operations that return a one-time secret (webhook creation and key rotation, checkout sessions, stream tokens) refuse to run unless you pass `--output-file PATH`, which writes the complete response to a new owner-only file, or `--show-secrets`. Treat your own metadata as potentially sensitive; redaction cannot recognise it.

API text is stripped of terminal control, bidirectional formatting, and invisible characters (including tag characters that can carry hidden text) in tables, notices, and errors.

Related: [payments](../guides/payments.md), [exit codes](../reference/exit-codes.md).
