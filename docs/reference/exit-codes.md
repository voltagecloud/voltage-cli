# Exit codes

| Code | Meaning |
|---|---|
| 0 | Success, or an accepted submission without waiting |
| 1 | The API rejected the request or a payment failed |
| 2 | Invalid invocation or configuration |
| 3 | Authentication or authorization failure |
| 4 | Transport failure or a submission with an unknown outcome |
| 5 | A wait or pagination deadline passed |
| 6 | A change was described but not sent: pass `--execute` or set `VOLTAGE_EXECUTE=1` |
| 130 | Interrupted |

After exit code 4 for a payment, check its ID before you retry; see [payment IDs](../concepts/safety-model.md#payment-ids-and-the-request-journal). After exit code 5, keep watching with `voltage payments get PAYMENT_ID --profile prod --wait completed`.

Many errors for exit codes 2 and 3 carry a `hint:` (or `error.hint` with `--json`) that names the next command: the discovery command for a missing `--org`, `--env`, or `--wallet`, the saved credential and profile names for an unknown or ambiguous selection, `voltage logout --local --account NAME` and then `voltage login --account NAME` for an expired or revoked login, `voltage logout --account NAME` and then `voltage login --account NAME --auth-url URL` to move a browser login to a different auth URL, `voltage auth status` for a rejected account credential (HTTP 401), and `voltage context` for an account credential that cannot reach the selected scope (HTTP 403). A rejected checkout token points to `--token-file` or the variable that supplies it, never to the account credential. A payment rejected with `quote_required` points to `voltage quotes create`.
