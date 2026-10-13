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

Errors for exit codes 2 and 3 carry a `hint:` (or `error.hint` with `--json`) that names the next command: the discovery command for a missing `--org`, `--env`, or `--wallet`, the saved credential and profile names for an unknown or ambiguous selection, `voltage login --account NAME` for an expired login, `voltage auth status` for a rejected credential (HTTP 401), and `voltage context` for a credential that cannot reach the selected scope (HTTP 403).
