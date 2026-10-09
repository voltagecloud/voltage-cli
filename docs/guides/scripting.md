# Scripting

On a terminal, results are tables; when piped, they are JSON. `--json` forces JSON. Scripts should read JSON: the [envelope](../reference/output.md#json) is stable, and the table layout may change between releases.

```sh
voltage payments list --profile prod --json | jq '.data.items[].status'
voltage context --json | jq -r '.data.organization_id.value'
```

## Send changes from a script

Commands that change something only describe their request unless you pass `--execute`, then exit with code 6 so a script that forgot it fails loudly. To send every change in a script or chain of commands, set `VOLTAGE_EXECUTE=1` (or `true`) instead:

```sh
VOLTAGE_EXECUTE=1 ./pay-invoices.sh
```

`0`, `false`, or an empty value keeps the default, and any other value fails every API command, reads included. The flag wins over the variable, a change sent because of the variable says so on stderr, and `voltage context` reports it as `execute_changes` with its source.

## Run without a person

- `--no-input` forbids the hidden API-key prompt even on a terminal. Supply credentials through the environment, a private file, or `--stdin` (for `auth import-key`).
- `-q, --quiet` suppresses optional status and notices, never requested results, errors, warnings, or recovery IDs.
- Diagnostics, prompts, and progress go to stderr, so stdout carries only results.
- Branch on [exit codes](../reference/exit-codes.md). Exit code 4 means the outcome is unknown: check the payment ID before you retry.

Related: [safety model](../concepts/safety-model.md), [output reference](../reference/output.md).
